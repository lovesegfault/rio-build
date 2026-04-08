//! narinfo signing + cross-tenant signature-visibility gate.
//!
//! Inherent methods on [`StoreServiceImpl`] used by PutPath
//! (sign-on-upload), PutPathBatch (resolve-once-sign-N), and the
//! read RPCs (sig-gate on QueryPathInfo/GetPath).

use super::*;

impl StoreServiceImpl {
    // r[impl store.substitute.tenant-sig-visibility]
    /// Cross-tenant sig-visibility gate. A substituted path (one that
    /// was NEVER built by any tenant — zero `path_tenants` rows) is
    /// visible to the requesting tenant only if one of its `signatures`
    /// verifies against the requesting tenant's trusted set: upstream
    /// `trusted_keys` ∪ the rio cluster key.
    ///
    /// Returns `true` if visible, `false` if hidden (caller returns
    /// NotFound). Unauthenticated requests (`tenant_id = None`) pass
    /// through — `r[store.tenant.narinfo-filter]` defines anonymous
    /// requests as unfiltered.
    ///
    /// # Substituted-path discriminator
    ///
    /// `path_tenants` is populated at build-completion by the scheduler
    /// (`upsert_path_tenants` in rio-scheduler/src/db/live_pins.rs).
    /// The Substituter does NOT populate it. So:
    ///   - ≥1 `path_tenants` row → someone built this → skip gate
    ///     (built paths are trusted regardless of signature)
    ///   - 0 rows → substitution-only → apply gate
    ///
    /// The PutPath→scheduler timing window (path IS built, count=0
    /// because the scheduler hasn't yet run `upsert_path_tenants`) is
    /// handled by the cluster-key union below: a rio-signed path
    /// always verifies against the cluster key, so it passes the gate
    /// regardless of the tenant's upstream config. Only paths signed
    /// ONLY by upstream keys the tenant doesn't trust are gated out.
    pub(super) async fn sig_visibility_gate(
        &self,
        tenant_id: Option<uuid::Uuid>,
        info: &ValidatedPathInfo,
    ) -> Result<bool, Status> {
        let Some(tid) = tenant_id else {
            return Ok(true); // anonymous → unfiltered
        };
        // No substituter → no substituted paths to gate.
        if self.substituter.is_none() {
            return Ok(true);
        }

        let path_hash = info.store_path.sha256_digest();

        // Two checks in one round-trip: does this tenant own it, and
        // has ANY tenant ever built it?
        let (owned, any_built): (bool, bool) = sqlx::query_as(
            "SELECT \
               bool_or(tenant_id = $2), \
               count(*) > 0 \
             FROM path_tenants WHERE store_path_hash = $1",
        )
        .bind(path_hash.as_slice())
        .bind(tid)
        .fetch_one(&self.pool)
        .await
        .map(|(o, a): (Option<bool>, bool)| (o.unwrap_or(false), a))
        .status_internal("sig_visibility_gate: path_tenants")?;

        if owned || any_built {
            // Built path (someone `path_tenants`'d it). Not
            // substitution-only → skip gate. The freshly-PutPath'd
            // case (count=0) falls through to the cluster-key union
            // below — NOT this branch.
            return Ok(true);
        }

        // Zero path_tenants rows → substitution-only path. Gate it.
        // r[impl store.substitute.tenant-sig-visibility]
        // Trusted = tenant's upstream pubkeys ∪ rio cluster key.
        // Without the cluster key, a freshly-built path (rio-signed,
        // path_tenants not yet populated by the scheduler) would be
        // gated as "untrusted substitution" and return NotFound to
        // its own tenant during the PutPath→scheduler window.
        let mut trusted = metadata::upstreams::tenant_trusted_keys(&self.pool, tid)
            .await
            .map_err(|e| metadata_status("sig_visibility_gate: trusted_keys", e))?;
        if let Some(ts) = &self.signer {
            trusted.push(ts.cluster().trusted_key_entry());
            // r[impl store.key.rotation-cluster-history]
            // Union prior cluster keys so paths signed under a
            // rotated-out key stay visible after CASCADE drops their
            // path_tenants rows. prior_cluster_entries is loaded once
            // at startup from cluster_key_history WHERE retired_at IS
            // NULL — no DB hit here.
            trusted.extend_from_slice(ts.prior_cluster_entries());
        }
        if trusted.is_empty() {
            // Tenant trusts no upstream keys AND no signer configured
            // → any substituted path is invisible. With a signer the
            // cluster key was pushed above, so this is the no-signer
            // edge only.
            return Ok(false);
        }

        let fp = rio_nix::narinfo::fingerprint(
            info.store_path.as_str(),
            &info.nar_hash,
            info.nar_size,
            &info
                .references
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>(),
        );
        Ok(crate::signing::any_sig_trusted(&info.signatures, &trusted, &fp).is_some())
    }

    /// Sync signing given a pre-resolved [`Signer`]. No DB hit.
    ///
    /// Extracted so PutPathBatch can resolve once + sign N times without
    /// N `get_active_signer` queries inside its phase-3 transaction.
    /// Holds the signature logic that was inlined in `maybe_sign`:
    /// empty-refs warn, fingerprint computation, `signer.sign()`,
    /// key-label debug line, push onto `info.signatures`.
    ///
    /// `was_tenant` drives the `key=tenant` vs `key=cluster` debug line;
    /// the caller passes whatever [`TenantSigner::resolve_once`] returned.
    pub(super) fn sign_with_resolved(
        &self,
        signer: &crate::signing::Signer,
        was_tenant: bool,
        info: &mut ValidatedPathInfo,
    ) {
        // r[impl store.signing.empty-refs-warn]
        // Defensive: a non-CA path with zero references is almost certainly
        // a worker that didn't scan (pre-fix upload.rs) or a scanning bug.
        // CA paths legitimately have empty refs (fetchurl, etc.). Don't block
        // the upload — just make noise so it's visible in logs/alerts.
        if info.content_address.is_none() && info.references.is_empty() {
            warn!(
                store_path = %info.store_path.as_str(),
                "signing non-CA path with zero references — suspicious for non-leaf derivation; \
                 GC will not protect deps (check worker ref-scanner)"
            );
            metrics::counter!("rio_store_sign_empty_refs_total").increment(1);
        }

        // References for the fingerprint are FULL store paths (not
        // basenames — that's a narinfo-text-format thing). ValidatedPathInfo
        // stores them as StorePath, which stringifies to full paths.
        let refs: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();

        let fp = rio_nix::narinfo::fingerprint(
            info.store_path.as_str(),
            &info.nar_hash,
            info.nar_size,
            &refs,
        );

        let sig = signer.sign(&fp);
        let key_label = if was_tenant { "tenant" } else { "cluster" };
        debug!(key = key_label, "signed narinfo fingerprint");
        info.signatures.push(sig);
    }

    // r[impl store.tenant.sign-key]
    /// If a signer is configured, compute the narinfo fingerprint and
    /// push a signature onto `info.signatures` using the tenant's key
    /// (or cluster fallback — see [`TenantSigner::resolve_once`]).
    ///
    /// Called just before complete_manifest_* writes narinfo to PG —
    /// the signature goes into the DB, and the HTTP cache server serves
    /// it as a `Sig:` line without ever touching the privkey.
    ///
    /// `tenant_id` comes from JWT `Claims.sub` (P0259 interceptor). `None`
    /// means: no JWT (dual-mode fallback), OR mTLS bypass (gateway cert,
    /// `nix copy` path — no per-build attribution), OR dev mode (no
    /// interceptor). All three correctly fall through to cluster key.
    ///
    /// Async because tenant-key resolution hits PG for the `tenant_keys`
    /// lookup when `tenant_id` is `Some`. For single-output paths
    /// (PutPath) that's fine — one query, not in a hot loop. Batch
    /// callers (PutPathBatch) should use [`TenantSigner::resolve_once`]
    /// then [`sign_with_resolved`](Self::sign_with_resolved) instead so
    /// the lookup happens once outside the transaction, not N times
    /// inside it.
    ///
    /// Error handling: `TenantKeyLookup` (the only failing variant — the
    /// `None` path is infallible) is logged + falls back to cluster key.
    /// A transient PG hiccup shouldn't fail the upload; the cluster sig
    /// is still valid, just not tenant-scoped. The caller gets a
    /// signature either way — `maybe_sign` itself stays infallible.
    pub(super) async fn maybe_sign(
        &self,
        tenant_id: Option<uuid::Uuid>,
        info: &mut ValidatedPathInfo,
    ) {
        let Some(ts) = &self.signer else {
            return;
        };

        let (signer, was_tenant) = match ts.resolve_once(tenant_id).await {
            Ok(pair) => pair,
            Err(e) => {
                // Transient PG failure — don't fail the upload. Fall back
                // to cluster key (sync, no DB hit). Log loud so ops
                // notices: a tenant WITH a configured key is now getting
                // cluster-signed paths, which `nix store verify
                // --trusted-public-keys tenant:<pk>` will reject. The
                // upload succeeds; the tenant's verify chain breaks.
                warn!(error = %e, ?tenant_id, "tenant-key lookup failed; falling back to cluster key");
                (ts.cluster().clone(), false)
            }
        };

        self.sign_with_resolved(&signer, was_tenant, info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::fixtures::{make_path_info, test_store_path};
    use tracing_test::traced_test;

    /// Build a StoreServiceImpl with a test signer but no DB/backend.
    /// These tests pass `tenant_id: None` to `maybe_sign`, so the pool
    /// inside `TenantSigner` is never queried — lazy connect stays lazy.
    /// (The Some-tenant path IS tested by the integration test at
    /// `tests/grpc/signing.rs`, which has a real PG.)
    fn svc_with_signer() -> StoreServiceImpl {
        // 32-byte seed → Signer::parse accepts `name:base64(seed)` (seed-only
        // form; ed25519 derives pubkey deterministically).
        let seed_b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0x42u8; 32]);
        let cluster = crate::signing::Signer::parse(&format!("test-key-1:{seed_b64}"))
            .expect("valid test signer");
        // Pool is lazy — never connects since these tests pass tenant_id=None
        // (the cluster-key path in resolve_once skips the DB entirely).
        let pool = PgPool::connect_lazy("postgres://unused").expect("lazy pool never connects");
        let ts = TenantSigner::new(cluster, pool.clone());
        StoreServiceImpl::new(pool).with_signer(ts)
    }

    /// r[verify store.signing.empty-refs-warn]
    /// Signing a non-CA path with zero references emits a warn! log
    /// containing "suspicious". The signing still proceeds (no block).
    #[tokio::test]
    #[traced_test]
    async fn maybe_sign_warns_on_empty_refs_non_ca() {
        let svc = svc_with_signer();
        // make_path_info gives: references=[], content_address=None. Exactly
        // the suspicious case.
        let mut info = make_path_info(&test_store_path("suspect"), b"nar", [0u8; 32]);
        assert!(info.references.is_empty());
        assert!(info.content_address.is_none());

        svc.maybe_sign(None, &mut info).await;

        assert!(
            logs_contain("suspicious"),
            "expected warn! with 'suspicious' in message"
        );
        assert!(
            logs_contain("zero references"),
            "expected warn! to mention zero references"
        );
        // Signing still happened — warn is observability only, not a block.
        assert_eq!(info.signatures.len(), 1, "signing should still proceed");
    }

    /// r[verify store.signing.empty-refs-warn]
    /// CA paths with empty refs do NOT warn (fetchurl etc. legitimately
    /// have no runtime deps).
    #[tokio::test]
    #[traced_test]
    async fn maybe_sign_no_warn_for_ca_path() {
        let svc = svc_with_signer();
        let mut info = make_path_info(&test_store_path("ca-path"), b"nar", [0u8; 32]);
        info.content_address = Some("fixed:r:sha256:abc".into());

        svc.maybe_sign(None, &mut info).await;

        assert!(
            !logs_contain("suspicious"),
            "CA path with empty refs should NOT warn"
        );
        assert_eq!(info.signatures.len(), 1);
    }

    /// r[verify store.signing.empty-refs-warn]
    /// Non-CA path WITH references does NOT warn (normal case).
    #[tokio::test]
    #[traced_test]
    async fn maybe_sign_no_warn_with_references() {
        let svc = svc_with_signer();
        let mut info = make_path_info(&test_store_path("normal"), b"nar", [0u8; 32]);
        info.references =
            vec![rio_nix::store_path::StorePath::parse(&test_store_path("dep-a")).unwrap()];

        svc.maybe_sign(None, &mut info).await;

        assert!(
            !logs_contain("suspicious"),
            "path with refs should NOT warn"
        );
        assert_eq!(info.signatures.len(), 1);
    }

    /// No signer configured → maybe_sign is a no-op. No warn emitted
    /// (the early return is BEFORE the check — intentional: unsigned
    /// stores don't cryptographically commit to the empty refs, so the
    /// blast radius is smaller).
    #[tokio::test]
    #[traced_test]
    async fn maybe_sign_noop_without_signer() {
        let pool = PgPool::connect_lazy("postgres://unused").unwrap();
        let svc = StoreServiceImpl::new(pool); // no .with_signer()
        let mut info = make_path_info(&test_store_path("unsigned"), b"nar", [0u8; 32]);

        svc.maybe_sign(None, &mut info).await;

        assert!(!logs_contain("suspicious"));
        assert!(info.signatures.is_empty(), "no signer → no signature");
    }

    // r[verify store.substitute.tenant-sig-visibility]
    /// The critical cross-tenant test: tenant A substitutes path P
    /// signed by key K. Tenant B (who also trusts K) sees P. Tenant C
    /// (who doesn't trust K) gets NotFound.
    #[tokio::test]
    async fn sig_visibility_gate_cross_tenant() {
        use crate::signing::Signer;
        use crate::test_helpers::seed_tenant;
        use rio_test_support::TestDb;

        let db = TestDb::new(&crate::MIGRATOR).await;
        // Gate only applies with substituter wired (`.is_none()`
        // short-circuits). The substituter itself won't be hit — the
        // path is pre-seeded, not miss-then-fetch.
        let sub = Arc::new(Substituter::new(db.pool.clone(), None));
        let svc = StoreServiceImpl::new(db.pool.clone()).with_substituter(sub);

        let tid_a = seed_tenant(&db.pool, "sig-gate-a").await;
        let tid_b = seed_tenant(&db.pool, "sig-gate-b").await;
        let tid_c = seed_tenant(&db.pool, "sig-gate-c").await;

        // Seed a path with a signature from key K.
        let seed_k = [0x77u8; 32];
        let signer_k = Signer::from_seed("key-K", &seed_k);
        let pk_k = ed25519_dalek::SigningKey::from_bytes(&seed_k).verifying_key();
        let trusted_k = format!(
            "key-K:{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, pk_k.as_bytes())
        );

        let path = test_store_path("cross-tenant-p");
        let (nar, nar_hash) = rio_test_support::fixtures::make_nar(b"xyz");
        let fp = rio_nix::narinfo::fingerprint(&path, &nar_hash, nar.len() as u64, &[]);
        let sig_k = signer_k.sign(&fp);

        // Seed the path in narinfo + manifests with K's sig — simulating
        // "tenant A substituted this from upstream K".
        let info = make_path_info(&path, &nar, nar_hash);
        let path_hash = info.store_path.sha256_digest();
        metadata::insert_manifest_uploading(&db.pool, &path_hash, &path, &[])
            .await
            .unwrap();
        let mut info_with_sig = info.clone();
        info_with_sig.signatures = vec![sig_k.clone()];
        info_with_sig.store_path_hash = path_hash.to_vec();
        metadata::complete_manifest_inline(&db.pool, &info_with_sig, nar.into())
            .await
            .unwrap();

        // — Tenant A substituted this (so: no path_tenants row, but
        //   A's upstream trusted_keys includes K) —
        // — Tenant B ALSO trusts K (different upstream URL, same key) —
        // — Tenant C trusts a DIFFERENT key J —
        // — Zero path_tenants rows: this is a substitution-only path —
        let _ = path_hash; // narinfo seeded above, hash no longer needed

        metadata::upstreams::insert(
            &db.pool,
            tid_a,
            "https://cache-k-a.example",
            50,
            std::slice::from_ref(&trusted_k),
            crate::metadata::SigMode::Keep,
        )
        .await
        .unwrap();

        // Tenant B trusts key K via an upstream config.
        metadata::upstreams::insert(
            &db.pool,
            tid_b,
            "https://cache-k.example",
            50,
            std::slice::from_ref(&trusted_k),
            crate::metadata::SigMode::Keep,
        )
        .await
        .unwrap();

        // Tenant C has an upstream but trusts a DIFFERENT key.
        metadata::upstreams::insert(
            &db.pool,
            tid_c,
            "https://cache-j.example",
            50,
            &["key-J:aaaa".into()],
            crate::metadata::SigMode::Keep,
        )
        .await
        .unwrap();

        let stored = metadata::query_path_info(&db.pool, &path)
            .await
            .unwrap()
            .unwrap();

        // A: trusts K (the substituting tenant) → sig verifies → visible.
        assert!(
            svc.sig_visibility_gate(Some(tid_a), &stored).await.unwrap(),
            "tenant A trusts K → visible"
        );

        // B: trusts K → sig verifies → visible.
        assert!(
            svc.sig_visibility_gate(Some(tid_b), &stored).await.unwrap(),
            "tenant B trusts K → visible"
        );

        // C: doesn't trust K → hidden.
        assert!(
            !svc.sig_visibility_gate(Some(tid_c), &stored).await.unwrap(),
            "tenant C doesn't trust K → NotFound"
        );

        // Anonymous → passes through (unfiltered per
        // r[store.tenant.narinfo-filter]).
        assert!(
            svc.sig_visibility_gate(None, &stored).await.unwrap(),
            "anonymous → unfiltered"
        );

        // — Built-path exemption: once ANY tenant has a path_tenants
        //   row, the gate is bypassed (built paths are trusted) —
        sqlx::query("INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2)")
            .bind(stored.store_path.sha256_digest().as_slice())
            .bind(tid_a)
            .execute(&db.pool)
            .await
            .unwrap();

        // Now even C (who doesn't trust K) sees it — built paths skip
        // the gate.
        assert!(
            svc.sig_visibility_gate(Some(tid_c), &stored).await.unwrap(),
            "built path (any path_tenants row) → gate bypassed"
        );
    }

    // r[verify store.key.rotation-cluster-history]
    /// Cluster-key rotation: path signed under old key A stays visible
    /// after rotating to key B + CASCADE deleting the owning tenant.
    ///
    /// Pre-fix: step 4 returns false. Gate derives key B from the
    /// current Signer only; sig was made by A; no path_tenants row left
    /// to bypass → path goes dark for every other tenant.
    ///
    /// Post-fix: prior_cluster carries A's pubkey entry → gate unions
    /// {B, A} into trusted → A-sig verifies.
    #[tokio::test]
    async fn sig_gate_survives_cluster_key_rotation_with_cascaded_tenant() {
        use crate::signing::{Signer, TenantSigner};
        use crate::test_helpers::seed_tenant;
        use rio_test_support::TestDb;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let sub = Arc::new(Substituter::new(db.pool.clone(), None));

        // — Cluster key A: the OLD key. Sign the path with this. —
        let seed_a = [0xAAu8; 32];
        let cluster_a = Signer::from_seed("rio-cluster-1", &seed_a);
        let entry_a = cluster_a.trusted_key_entry();

        // — Cluster key B: the NEW key. Active Signer post-rotation. —
        let seed_b = [0xBBu8; 32];
        let cluster_b = Signer::from_seed("rio-cluster-2", &seed_b);

        assert_ne!(seed_a, seed_b, "precondition: distinct keys");
        assert_ne!(
            entry_a,
            cluster_b.trusted_key_entry(),
            "precondition: distinct trusted-key entries"
        );

        // 1. Seed path signed by cluster key A (no tenant sig, no
        //    upstream sig — pure rio-signed built path).
        let path = test_store_path("rotation-survivor");
        let (nar, nar_hash) = rio_test_support::fixtures::make_nar(b"rot");
        let fp = rio_nix::narinfo::fingerprint(&path, &nar_hash, nar.len() as u64, &[]);
        let sig_a = cluster_a.sign(&fp);

        let info = make_path_info(&path, &nar, nar_hash);
        let path_hash = info.store_path.sha256_digest();
        metadata::insert_manifest_uploading(&db.pool, &path_hash, &path, &[])
            .await
            .unwrap();
        let mut info_with_sig = info.clone();
        info_with_sig.signatures = vec![sig_a];
        info_with_sig.store_path_hash = path_hash.to_vec();
        metadata::complete_manifest_inline(&db.pool, &info_with_sig, nar.into())
            .await
            .unwrap();

        // 2. Seed path_tenants row for tenant T (path was "built by T").
        let tid_t = seed_tenant(&db.pool, "rotation-owner").await;
        sqlx::query("INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2)")
            .bind(path_hash.as_slice())
            .bind(tid_t)
            .execute(&db.pool)
            .await
            .unwrap();

        // 3. Rotate: active Signer = B, prior_cluster = [A's entry].
        //    Route I — via with_prior_cluster (equivalent to what
        //    main.rs does via load_prior_cluster at startup after an
        //    operator inserts A into cluster_key_history).
        let ts_rotated =
            TenantSigner::new(cluster_b, db.pool.clone()).with_prior_cluster(vec![entry_a]);
        let svc = StoreServiceImpl::new(db.pool.clone())
            .with_substituter(sub.clone())
            .with_signer(ts_rotated);

        // 4. CASCADE: delete tenant T → path_tenants row drops. The
        //    path is now path_tenants-orphaned: gate re-fires on the
        //    next read.
        sqlx::query("DELETE FROM tenants WHERE tenant_id = $1")
            .bind(tid_t)
            .execute(&db.pool)
            .await
            .unwrap();
        // Verify CASCADE actually dropped the row (belt-and-suspenders —
        // migration 012's ON DELETE CASCADE is what we're relying on).
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM path_tenants WHERE store_path_hash = $1")
                .bind(path_hash.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(n, 0, "CASCADE should have dropped path_tenants row");

        let stored = metadata::query_path_info(&db.pool, &path)
            .await
            .unwrap()
            .unwrap();

        // 5. Other tenant queries → visible (prior_cluster carries A).
        let tid_other = seed_tenant(&db.pool, "rotation-reader").await;
        assert!(
            svc.sig_visibility_gate(Some(tid_other), &stored)
                .await
                .unwrap(),
            "path signed under old cluster key A MUST stay visible after \
             rotation to B when A is in prior_cluster — this is the \
             CASCADE-survival property"
        );

        // — Negative control: same rotation WITHOUT prior_cluster →
        //   path goes dark. Proves the test isn't passing for the
        //   wrong reason (e.g. some other bypass). —
        let ts_no_history =
            TenantSigner::new(Signer::from_seed("rio-cluster-2", &seed_b), db.pool.clone());
        let svc_no_history = StoreServiceImpl::new(db.pool.clone())
            .with_substituter(sub)
            .with_signer(ts_no_history);
        assert!(
            !svc_no_history
                .sig_visibility_gate(Some(tid_other), &stored)
                .await
                .unwrap(),
            "negative control: WITHOUT prior_cluster, old-key path MUST \
             be invisible (this is the bug P0521 fixes)"
        );
    }
}
