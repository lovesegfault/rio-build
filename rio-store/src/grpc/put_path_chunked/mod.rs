//! `PutPathChunked` (ADR-022 §6): builder-side chunked output upload.
//!
//! The RPC is a client stream — one [`PutPathChunkedBegin`] frame
//! carrying every output's metadata, Directory bodies, and chunk
//! manifest, followed by one `Chunk` frame per `Begin.novel` digest.
//! The handler is three phases, each a submodule:
//!
//! - [`validate`] — pure validation of the `Begin` frame against the
//!   caller's HMAC claims (`r[store.put.chunked-bounds]`). No DB, no
//!   S3; runs before any placeholder claim or side effect.
//! - [`verify`] — the §6.3 sequential verify task: regenerate the NAR
//!   framing from the validated Directory tree, splice each chunk body
//!   in (novel chunks from the stream, deduped chunks from the CAS),
//!   recompute SHA-256 + the reference scan + per-file BLAKE3, and
//!   compare against the claimed values.
//! - [`commit`] — derive the castore index from the validated tree;
//!   the single-transaction §6.2 commit across all non-skipped outputs
//!   lives in [`StoreServiceImpl::commit_chunked`].
//!
//! Unlike `PutPath`/`PutPathBatch` there is no whole-NAR buffer and no
//! `nar_bytes_budget` charge — see the [`verify`] module docs.
//!
//! [`PutPathChunkedBegin`]: rio_proto::types::PutPathChunkedBegin

use std::collections::HashMap;

use tonic::{Request, Response, Status, Streaming};
use tracing::warn;

use rio_auth::hmac::AssignmentClaims;
use rio_nix::nar::MAX_NAR_ENTRIES;
use rio_nix::store_path::StorePath;
use rio_proto::types::{
    PutPathChunkedBegin, PutPathChunkedRequest, PutPathChunkedResponse, put_path_chunked_request,
};
use rio_proto::validated::ValidatedPathInfo;

use crate::cas;
use crate::metadata;

use super::put_path::PlaceholderClaim;
use super::put_path::common::{PlaceholderGuard, drain_stream};
use super::{StoreServiceImpl, putpath_metadata_status};

pub(crate) mod validate;

mod commit;
mod verify;

use validate::{ValidatedBegin, validate_begin};
use verify::{MismatchReason, Verdict};

/// Claims to validate a `Begin` frame against when
/// `verify_assignment_token` returned `Ok(None)` (dev mode — no HMAC
/// verifier configured — or a service-token bypass caller).
///
/// Trusts the message for exactly the fields the token would otherwise
/// attest: the deriver binding (`drv_hash` echoes the deriver's hash
/// part, so the binding check is a tautology), the output-path
/// allowlist (`expected_outputs` echoes the outputs), and the
/// input-closure attestation (empty digest → unattested → the echo is
/// accepted). Every *structural* check in [`validate_begin`] — bounds,
/// directory reachability, chunk-run alignment, `novel` ordering —
/// still runs. This is the same trust posture `PutPath`'s dev mode
/// has: authorization waived, integrity not.
fn synthesize_claims(begin: &PutPathChunkedBegin) -> AssignmentClaims {
    AssignmentClaims {
        executor_id: "dev".into(),
        // An unparseable deriver yields an empty drv_hash; validate_begin
        // then rejects the deriver itself with INVALID_ARGUMENT.
        drv_hash: StorePath::parse(&begin.deriver)
            .map(|p| p.hash_part())
            .unwrap_or_default(),
        expected_outputs: begin.outputs.iter().map(|o| o.store_path.clone()).collect(),
        is_ca: false,
        expiry_unix: u64::MAX,
        tenant: None,
        input_closure_digest: String::new(),
    }
}

/// Per-output placeholder state carried from phase A into the commit.
struct OutputClaim {
    /// `sha256(store_path)` — the manifests/narinfo PK.
    store_path_hash: Vec<u8>,
    /// `Some(claim_id)` when this handler owns an `'uploading'`
    /// placeholder for the output; `None` when the output was
    /// idempotent-skipped (already `'complete'`).
    claim: Option<uuid::Uuid>,
}

impl StoreServiceImpl {
    /// ADR-022 §6 chunked output upload: validate → placeholders →
    /// write-ahead chunk rows → sequential verify walk → CA recompute →
    /// one commit transaction.
    // r[impl store.put.chunked]
    // r[impl store.atomic.multi-output]
    pub(super) async fn put_path_chunked_impl(
        &self,
        request: Request<Streaming<PutPathChunkedRequest>>,
    ) -> Result<Response<PutPathChunkedResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // Same SLI as PutPath/PutPathBatch — one upload-latency
        // histogram regardless of which RPC carried the bytes.
        let start = std::time::Instant::now();
        let _duration_guard = scopeguard::guard((), move |()| {
            metrics::histogram!("rio_store_put_path_duration_seconds")
                .record(start.elapsed().as_secs_f64());
        });

        let auth = self.authorize(&request)?;
        let mut stream = request.into_inner();

        // First frame MUST be Begin.
        let begin = match stream.message().await? {
            Some(PutPathChunkedRequest {
                msg: Some(put_path_chunked_request::Msg::Begin(b)),
            }) => b,
            Some(_) => {
                return Err(Status::invalid_argument(
                    "PutPathChunked: first frame must be Begin",
                ));
            }
            None => {
                return Err(Status::invalid_argument("PutPathChunked: empty stream"));
            }
        };

        let claims = match &auth.hmac_claims {
            Some(c) => c.clone(),
            None => synthesize_claims(&begin),
        };
        let validated = validate_begin(&begin, &claims)?;
        let n_outputs = validated.outputs.len();

        // r[impl store.castore.tenant-scope]
        // r[impl store.put.tenant-junction]
        // Tenant for the `path_tenants` junction writes. The production
        // caller is the builder, which authenticates with the HMAC
        // assignment token only (no gateway JWT), so this MUST fall
        // back to `claims.tenant` — the same shared resolution the
        // castore read side uses — or every junction write in this RPC
        // is a no-op for the builder and the just-committed outputs are
        // invisible to (and unpinned for) their own tenant until the
        // scheduler's best-effort completion upsert. PutPath and
        // PutPathBatch resolve their junction tenant the same way.
        // Signing-key selection deliberately keeps using the JWT-only
        // `auth.tenant_id` (unchanged behavior).
        let junction_tenant = super::resolve_tenant_id(auth.tenant_id, auth.hmac_claims.as_ref());

        // The backend and cache are constructed together (`None` iff the
        // other is `None` — see `with_chunk_cache`), so one gate covers
        // both: the verify walk PUTs novel chunks to the backend and
        // fetches deduped ones through the cache.
        let (Some(backend), Some(chunk_cache)) =
            (self.chunk_backend.clone(), self.chunk_cache.clone())
        else {
            return Err(Status::failed_precondition(
                "PutPathChunked requires a chunk backend",
            ));
        };

        // Error-metric unit is per store path, mirroring PutPathBatch:
        // outputs already resolved as `exists` are not re-counted as
        // errors.
        let mut n_exists_emitted = 0usize;
        macro_rules! bail {
            ($status:expr) => {{
                metrics::counter!("rio_store_put_path_total", "result" => "error")
                    .increment((n_outputs - n_exists_emitted) as u64);
                return Err($status);
            }};
        }

        // ── Phase A: placeholders (non-CA only). ─────────────────────
        let mut guards: Vec<PlaceholderGuard> = Vec::new();
        let mut output_claims: Vec<OutputClaim> = Vec::with_capacity(n_outputs);
        let mut skipped = vec![false; n_outputs];
        for (i, out) in validated.outputs.iter().enumerate() {
            let store_path_hash = out.store_path.sha256_digest().to_vec();
            // r[impl sec.authz.ca-path-derived+2]
            // CA outputs claim no placeholder before the server-side
            // CA-path recompute (§6.3) — phase C claims them.
            if out.is_ca {
                output_claims.push(OutputClaim {
                    store_path_hash,
                    claim: None,
                });
                continue;
            }
            let refs_str: Vec<String> = out.references.iter().map(|r| r.to_string()).collect();
            match self
                .claim_placeholder(
                    &store_path_hash,
                    out.store_path.as_str(),
                    &refs_str,
                    "PutPathChunked",
                )
                .await
            {
                Ok(PlaceholderClaim::AlreadyComplete) => {
                    skipped[i] = true;
                    n_exists_emitted += 1;
                    output_claims.push(OutputClaim {
                        store_path_hash,
                        claim: None,
                    });
                }
                Ok(PlaceholderClaim::Owned(c)) => {
                    guards.push(self.spawn_placeholder_guard(store_path_hash.clone(), c));
                    output_claims.push(OutputClaim {
                        store_path_hash,
                        claim: Some(c),
                    });
                }
                // r[impl store.put.concurrent-wait]
                // Same bounded wait as PutPath: the builder's client-side
                // retry has the same budget-vs-upload-duration mismatch
                // the gateway's does.
                Ok(PlaceholderClaim::Concurrent) => match self
                    .wait_for_concurrent_upload(
                        &store_path_hash,
                        out.store_path.as_str(),
                        &refs_str,
                    )
                    .await
                {
                    Ok(PlaceholderClaim::AlreadyComplete) => {
                        metrics::counter!("rio_store_put_path_total", "result" => "exists")
                            .increment(1);
                        skipped[i] = true;
                        n_exists_emitted += 1;
                        output_claims.push(OutputClaim {
                            store_path_hash,
                            claim: None,
                        });
                    }
                    Ok(PlaceholderClaim::Owned(c)) => {
                        guards.push(self.spawn_placeholder_guard(store_path_hash.clone(), c));
                        output_claims.push(OutputClaim {
                            store_path_hash,
                            claim: Some(c),
                        });
                    }
                    Ok(PlaceholderClaim::Concurrent) => {
                        drain_stream("PutPathChunked", &mut stream).await;
                        bail!(Status::aborted(format!(
                            "PutPathChunked: outputs[{i}]: {}; retry",
                            rio_proto::CONCURRENT_PUTPATH_MSG
                        )));
                    }
                    Err(e) => {
                        drain_stream("PutPathChunked", &mut stream).await;
                        bail!(putpath_metadata_status(
                            "PutPathChunked: concurrent-wait",
                            e
                        ));
                    }
                },
                Err(e) => {
                    drain_stream("PutPathChunked", &mut stream).await;
                    bail!(putpath_metadata_status(
                        "PutPathChunked: claim_placeholder",
                        e
                    ));
                }
            }
        }

        // Every output already complete → nothing to verify or commit.
        // The remaining Chunk frames carry content that is already
        // durable via the existing manifests; discard them. The tenant
        // junctions still have to be written — the prior commit may
        // have been another tenant's, and without a `path_tenants` row
        // this caller can neither read the path's castore
        // (`r[store.castore.tenant-scope]`) nor pin it against the
        // other tenant's GC retention window lapsing.
        if skipped.iter().all(|s| *s) {
            drain_stream("PutPathChunked", &mut stream).await;
            if let Err(e) = self
                .insert_path_tenants_for_all(&output_claims, junction_tenant)
                .await
            {
                bail!(putpath_metadata_status("PutPathChunked: path_tenants", e));
            }
            return Ok(Response::new(PutPathChunkedResponse {
                created: vec![false; n_outputs],
            }));
        }

        // ── Phase B: write-ahead chunk rows, then the verify walk. ───
        // r[impl store.chunk.self-verify]
        if let Err(e) = self.insert_pending_chunks(&validated).await {
            drain_stream("PutPathChunked", &mut stream).await;
            bail!(putpath_metadata_status(
                "PutPathChunked: insert_pending_chunks",
                e
            ));
        }

        let verdict = match verify::run_verify(
            &mut stream,
            &validated,
            &skipped,
            &backend,
            &chunk_cache,
            MAX_NAR_ENTRIES,
            self.chunk_upload_max_concurrent,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => bail!(e),
        };

        let (computed, uploaded) = match verdict {
            Verdict::Match { computed, uploaded } => (computed, uploaded),
            Verdict::Mismatch { output_idx, reason } => {
                let out = &validated.outputs[output_idx];
                warn!(
                    store_path = %out.store_path,
                    deriver = %begin.deriver,
                    reason = reason.as_str(),
                    claimed_nar_hash = %hex::encode(out.nar_hash),
                    claimed_refs = ?out.references.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
                    "PutPathChunked: verify mismatch — builder claim does not match recomputed value",
                );
                match reason {
                    MismatchReason::Refs => {
                        metrics::counter!("rio_store_refs_mismatch_total").increment(1);
                    }
                    _ => {
                        metrics::counter!("rio_store_narhash_mismatch_total").increment(1);
                    }
                }
                bail!(Status::failed_precondition(format!(
                    "PutPathChunked: outputs[{output_idx}] ({}) failed verification: {} \
                     does not match the server-recomputed value",
                    out.store_path,
                    reason.as_str(),
                )));
            }
            Verdict::Incomplete => {
                metrics::counter!("rio_store_putpath_incomplete_total").increment(1);
                bail!(Status::failed_precondition(
                    "PutPathChunked: stream ended before all novel chunks were received",
                ));
            }
            Verdict::Unavailable(msg) => {
                metrics::counter!("rio_store_putpath_verify_unavailable_total").increment(1);
                bail!(Status::unavailable(format!("PutPathChunked: {msg}")));
            }
        };

        // ── Phase C: CA recompute → CA placeholders → ONE commit tx. ─
        if let Err(e) = self
            .claim_ca_placeholders(
                &validated,
                &computed,
                &mut skipped,
                &mut output_claims,
                &mut guards,
                &mut n_exists_emitted,
            )
            .await
        {
            bail!(e);
        }
        // A CA re-upload can flip every output to skipped here too —
        // same tenant-junction obligation as the phase-A early return.
        if skipped.iter().all(|s| *s) {
            if let Err(e) = self
                .insert_path_tenants_for_all(&output_claims, junction_tenant)
                .await
            {
                bail!(putpath_metadata_status("PutPathChunked: path_tenants", e));
            }
            return Ok(Response::new(PutPathChunkedResponse {
                created: vec![false; n_outputs],
            }));
        }

        let resolved_signer = self.resolve_batch_signer(auth.tenant_id).await;
        let created = match self
            .commit_chunked(
                &validated,
                &computed,
                &skipped,
                &output_claims,
                &begin.deriver,
                &uploaded,
                resolved_signer.as_ref(),
                junction_tenant,
            )
            .await
        {
            Ok(c) => c,
            Err(e) => bail!(e),
        };

        for g in guards {
            g.defuse();
        }
        Ok(Response::new(PutPathChunkedResponse { created }))
    }

    /// Tenant junctions for every output, outside any transaction.
    /// Used by the all-outputs-skipped early returns, where there is no
    /// commit transaction to carry the inserts; the spec requires the
    /// junction for idempotent-skipped outputs too (the prior commit
    /// may belong to another tenant or predate tenancy). Idempotent; a
    /// `None` tenant writes nothing.
    // r[impl store.castore.tenant-scope]
    // r[impl store.put.tenant-junction]
    async fn insert_path_tenants_for_all(
        &self,
        output_claims: &[OutputClaim],
        tenant_id: Option<uuid::Uuid>,
    ) -> Result<(), metadata::MetadataError> {
        if tenant_id.is_none() {
            return Ok(());
        }
        let mut conn = self.pool.acquire().await?;
        for oc in output_claims {
            match metadata::insert_path_tenant_in_conn(&mut conn, &oc.store_path_hash, tenant_id)
                .await
            {
                // The assignment token names a tenant deleted while the
                // build was in flight (path_tenants.tenant_id → tenants
                // is ON DELETE CASCADE, so the FK insert is what trips).
                // Skip the junction writes — a row for a deleted tenant
                // is meaningless, the already-durable content stays
                // valid, and the un-pinned path simply ages out via
                // normal GC retention. Every remaining output names the
                // same tenant, so stop after the first hit. Each insert
                // here is its own implicit transaction (no surrounding
                // tx to poison), so a plain catch is enough.
                Err(e) if metadata::is_deleted_tenant_fk(&e) => {
                    warn!(
                        tenant_id = %tenant_id.expect("checked non-None above"),
                        "PutPathChunked: path_tenants junction skipped — tenant was deleted \
                         while the build was in flight"
                    );
                    return Ok(());
                }
                other => other?,
            }
        }
        Ok(())
    }

    /// Phase B write-ahead: insert a `refcount = 0` row for every
    /// `novel` digest before any S3 PUT. Sizes come from the first
    /// occurrence in any output's manifest (cross-occurrence agreement
    /// was enforced by `validate_begin`).
    async fn insert_pending_chunks(
        &self,
        validated: &ValidatedBegin,
    ) -> Result<(), metadata::MetadataError> {
        let novel_set: std::collections::HashSet<&[u8; 32]> = validated.novel.iter().collect();
        let mut sizes: HashMap<[u8; 32], u32> = HashMap::with_capacity(validated.novel.len());
        for out in &validated.outputs {
            for (d, s) in &out.chunk_manifest {
                if novel_set.contains(d) {
                    sizes.entry(*d).or_insert(*s);
                }
            }
        }
        let rows: Vec<([u8; 32], u32)> = validated
            .novel
            .iter()
            .map(|d| (*d, *sizes.get(d).expect("novel ⊆ chunk_manifest digests")))
            .collect();
        metadata::insert_pending_chunks(&self.pool, &rows).await
    }

    /// Phase C CA gate: recompute each CA output's store path from the
    /// **server-computed** NAR hash and reject on mismatch, then claim
    /// its placeholder.
    ///
    /// The spec's single-`INSERT … ON CONFLICT … RETURNING` form is
    /// replaced by the same `claim_placeholder` state machine the
    /// non-CA path uses; the guarantee is identical — the loser of a
    /// concurrent CA commit never enters the commit transaction, so
    /// refcounts are never double-counted — and the placeholder gets
    /// the same heartbeat/drop-reap lifecycle as every other writer's.
    // r[impl store.put.chunked-ca]
    // r[impl sec.authz.ca-path-derived+2]
    async fn claim_ca_placeholders(
        &self,
        validated: &ValidatedBegin,
        computed: &[Option<verify::OutputComputed>],
        skipped: &mut [bool],
        output_claims: &mut [OutputClaim],
        guards: &mut Vec<PlaceholderGuard>,
        n_exists_emitted: &mut usize,
    ) -> Result<(), Status> {
        for (i, out) in validated.outputs.iter().enumerate() {
            if !out.is_ca {
                continue;
            }
            let comp = computed[i]
                .as_ref()
                .expect("CA outputs are never idempotent-skipped before the recompute");
            // Self-reference: `make_fixed_output` has no `:self` token
            // support yet — mirror `verify_ca_store_path`'s explicit
            // rejection rather than silently deriving a wrong path.
            let refs: Vec<StorePath> = out
                .references
                .iter()
                .filter(|r| r.as_str() != out.store_path.as_str())
                .cloned()
                .collect();
            if refs.len() != out.references.len() {
                return Err(Status::unimplemented(
                    "PutPathChunked: self-referencing floating-CA not yet supported \
                     (extend make_fixed_output with :self)",
                ));
            }
            let nar_hash = rio_nix::hash::NixHash::new(
                rio_nix::hash::HashAlgo::SHA256,
                comp.nar_hash.to_vec(),
            )
            .map_err(|e| Status::internal(format!("PutPathChunked: nar_hash construct: {e}")))?;
            let expected = StorePath::make_fixed_output(
                out.store_path.name(),
                &nar_hash,
                /* recursive */ true,
                &refs,
            )
            .map_err(|e| {
                Status::invalid_argument(format!("PutPathChunked: CA path derive: {e}"))
            })?;
            if expected.as_str() != out.store_path.as_str() {
                warn!(
                    store_path = %out.store_path,
                    expected = %expected,
                    "PutPathChunked: is_ca store_path does not match server-derived CA path"
                );
                metrics::counter!(
                    "rio_store_hmac_rejected_total",
                    "reason" => "ca_path_mismatch"
                )
                .increment(1);
                return Err(Status::permission_denied(format!(
                    "PutPathChunked: outputs[{i}] store_path does not match the \
                     content-derived CA path"
                )));
            }

            let refs_str: Vec<String> = out.references.iter().map(|r| r.to_string()).collect();
            match self
                .claim_placeholder(
                    &output_claims[i].store_path_hash,
                    out.store_path.as_str(),
                    &refs_str,
                    "PutPathChunked",
                )
                .await
            {
                Ok(PlaceholderClaim::AlreadyComplete) => {
                    skipped[i] = true;
                    *n_exists_emitted += 1;
                }
                Ok(PlaceholderClaim::Owned(c)) => {
                    guards.push(
                        self.spawn_placeholder_guard(output_claims[i].store_path_hash.clone(), c),
                    );
                    output_claims[i].claim = Some(c);
                }
                Ok(PlaceholderClaim::Concurrent) => {
                    // Stream is fully consumed by now — no drain needed.
                    return Err(Status::aborted(format!(
                        "PutPathChunked: outputs[{i}]: {}; retry",
                        rio_proto::CONCURRENT_PUTPATH_MSG
                    )));
                }
                Err(e) => {
                    return Err(putpath_metadata_status(
                        "PutPathChunked: claim_placeholder (CA)",
                        e,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Phase C: ONE transaction across every non-skipped output —
    /// manifest_data, chunk refcounts, status flip, narinfo, castore
    /// index, tenant junctions, `uploaded_at` for the chunks this
    /// stream PUT to S3. Tenant junctions are also written for
    /// idempotent-skipped outputs (the prior commit may have been via
    /// legacy `PutPath`, which didn't write them).
    // r[impl store.atomic.multi-output]
    // r[impl obs.metric.transfer-volume]
    #[allow(clippy::too_many_arguments)]
    async fn commit_chunked(
        &self,
        validated: &ValidatedBegin,
        computed: &[Option<verify::OutputComputed>],
        skipped: &[bool],
        output_claims: &[OutputClaim],
        deriver: &str,
        uploaded: &std::collections::HashSet<[u8; 32]>,
        resolved_signer: Option<&(crate::signing::Signer, bool)>,
        tenant_id: Option<uuid::Uuid>,
    ) -> Result<Vec<bool>, Status> {
        let registration_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let deriver = StorePath::parse(deriver).ok();

        // Derive the castore index for every output that will commit
        // BEFORE opening the transaction — a prost encode per directory
        // plus the entry walk is CPU work that has no business holding
        // row locks.
        let mut parsed: Vec<Option<cas::ParsedNar>> = Vec::with_capacity(validated.outputs.len());
        for (i, out) in validated.outputs.iter().enumerate() {
            if skipped[i] {
                parsed.push(None);
            } else {
                parsed.push(Some(cas::cpu_bound(|| {
                    commit::build_parsed(out, &validated.directories, MAX_NAR_ENTRIES)
                })?));
            }
        }

        // Union of every non-skipped output's manifest digests —
        // `lock_chunks_for_commit` (see its doc for the lock-order and
        // presence-proof contract) unions in `uploaded` and takes ONE
        // sorted FOR UPDATE over the whole set up front.
        let all_digests: Vec<Vec<u8>> = {
            let mut set = std::collections::BTreeSet::new();
            for (i, out) in validated.outputs.iter().enumerate() {
                if !skipped[i] {
                    set.extend(out.chunk_manifest.iter().map(|(d, _)| d.to_vec()));
                }
            }
            set.into_iter().collect()
        };

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| rio_common::grpc::internal("PutPathChunked: begin transaction", e))?;

        // r[impl store.chunk.durable-flag]
        // The verify walk proved it COULD obtain each chunk's bytes
        // earlier; this proves the object is still claimable now that
        // the row is locked. A GC-claimed chunk fails the proof —
        // committing anyway is the I-201 lie through the GC round-trip
        // (see `lock_chunks_for_commit`'s presence-proof job).
        match metadata::lock_chunks_for_commit(&mut tx, &all_digests, uploaded).await {
            Ok(unproven) if unproven.is_empty() => {}
            Ok(unproven) => {
                drop(tx);
                metrics::counter!("rio_store_putpath_verify_unavailable_total").increment(1);
                return Err(Status::unavailable(format!(
                    "PutPathChunked: {} referenced chunk(s) were reclaimed by GC during the \
                     upload (first: {}); retry",
                    unproven.len(),
                    hex::encode(&unproven[0]),
                )));
            }
            Err(e) => {
                drop(tx);
                return Err(putpath_metadata_status("PutPathChunked: lock chunks", e));
            }
        }

        let mut created = vec![false; validated.outputs.len()];
        let mut infos: Vec<Option<ValidatedPathInfo>> = Vec::with_capacity(validated.outputs.len());
        for (i, out) in validated.outputs.iter().enumerate() {
            if skipped[i] {
                // r[impl store.castore.tenant-scope]
                // Tolerant variant: a tenant deleted while the build
                // was in flight skips the junction write (warned + no
                // row) instead of aborting the other outputs' commit.
                if let Err(e) = metadata::insert_path_tenant_skipping_deleted_in_tx(
                    &mut tx,
                    &output_claims[i].store_path_hash,
                    tenant_id,
                )
                .await
                {
                    drop(tx);
                    return Err(putpath_metadata_status("PutPathChunked: path_tenants", e));
                }
                infos.push(None);
                continue;
            }
            let comp = computed[i]
                .as_ref()
                .expect("non-skipped outputs always have a computed verdict");
            let mut info = ValidatedPathInfo {
                store_path: out.store_path.clone(),
                store_path_hash: output_claims[i].store_path_hash.clone(),
                deriver: deriver.clone(),
                nar_hash: comp.nar_hash,
                nar_size: comp.nar_size,
                // Equal to `comp.references` at this point (the verify
                // walk rejected any disagreement); keep the parsed
                // StorePath form rather than re-parsing the strings.
                references: out.references.clone(),
                registration_time,
                ultimate: false,
                signatures: Vec::new(),
                content_address: None,
            };
            if let Some((signer, was_tenant)) = resolved_signer {
                self.sign_with_resolved(signer, *was_tenant, &mut info);
            }

            // One refcount per distinct chunk per manifest, digest-
            // sorted (`r[store.chunk.lock-order]`) — the BTreeMap dedups
            // by digest and iterates in ascending key order.
            // First-vs-last occurrence is moot because validate_begin
            // already rejected a digest claimed at two different sizes.
            // The chunk rows are already locked by
            // `lock_chunks_for_commit` above, so the per-output order
            // here cannot deadlock another writer.
            // TODO: the per-output `directories` refcount UPSERTs inside
            // `set_nar_index_in_conn` are each sorted but not globally
            // ordered across outputs — two concurrent multi-output
            // commits sharing Directory bodies can still 40P01. Shared
            // with PutPathBatch (same per-output complete loop); fix
            // both by aggregating into one sorted upsert per table.
            let distinct: std::collections::BTreeMap<[u8; 32], u32> =
                out.chunk_manifest.iter().copied().collect();
            let (chunk_hashes, chunk_sizes): (Vec<Vec<u8>>, Vec<i64>) = distinct
                .into_iter()
                .map(|(d, s)| (d.to_vec(), i64::from(s)))
                .unzip();

            let manifest_bytes = commit::build_manifest(out);
            if let Err(e) = metadata::commit_chunked_output_in_conn(
                &mut tx,
                &info,
                output_claims[i]
                    .claim
                    .expect("non-skipped outputs own a placeholder"),
                &manifest_bytes,
                &chunk_hashes,
                &chunk_sizes,
                parsed[i].as_ref().expect("built above for non-skipped"),
                tenant_id,
            )
            .await
            {
                drop(tx);
                return Err(putpath_metadata_status("PutPathChunked: commit output", e));
            }
            created[i] = true;
            infos.push(Some(info));
        }

        // The novel chunks this stream actually PUT are now referenced
        // by at least one complete manifest — record S3 presence so
        // later uploads dedup against them.
        let uploaded_vecs: Vec<Vec<u8>> = uploaded.iter().map(|d| d.to_vec()).collect();
        if let Err(e) = metadata::mark_chunks_uploaded_in_conn(&mut tx, &uploaded_vecs).await {
            drop(tx);
            return Err(putpath_metadata_status(
                "PutPathChunked: mark_chunks_uploaded",
                e,
            ));
        }

        tx.commit()
            .await
            .map_err(|e| rio_common::grpc::internal("PutPathChunked: commit", e))?;

        for info in infos.into_iter().flatten() {
            metrics::counter!("rio_store_put_path_total", "result" => "created").increment(1);
            metrics::counter!("rio_store_put_path_bytes_total").increment(info.nar_size);
        }
        Ok(created)
    }
}
