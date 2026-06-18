//! Concurrent same-path `PutPath` contention — `r[store.put.concurrent-wait]`.
//!
//! The production failure these tests guard: two clients race the same
//! store path; the loser used to get `ABORTED "concurrent PutPath in
//! progress … retry"` immediately. The gateway's buffered re-send retry
//! (~6 s budget, tuned for KB-sized `.drv` NARs) cannot cover a winner
//! that streams a large NAR for tens of seconds, and the gateway's
//! streaming path cannot retry at all (bytes already consumed) — so the
//! "retry" in the message was a lie and the client's whole
//! `wopAddMultipleToStore` died. Reproduced live with two tunnels racing
//! one path. The fix: the store waits (bounded) for the in-flight upload
//! and then takes the idempotent-skip — or claim-takeover — path.

use super::*;

use rio_auth::hmac::{AssignmentClaims, HmacSigner, HmacVerifier};
use rio_proto::types::PutPathTrailer;
use rio_store::test_helpers::{path_hash, seed_tenant};
use rio_test_support::metrics::CountingRecorder;

const KEY: &[u8] = b"concurrent-test-hmac-key-32-byte";

/// HMAC assignment token authorizing `outputs` for `tenant` — the
/// builder/worker identity shape (no JWT anywhere). Same shape as
/// tenancy.rs's helper.
fn token_for(tenant: uuid::Uuid, outputs: Vec<String>) -> String {
    HmacSigner::from_key(KEY.to_vec()).sign(&AssignmentClaims {
        executor_id: "concurrent-test".into(),
        drv_hash: "00".repeat(32),
        expected_outputs: outputs,
        is_ca: false,
        expiry_unix: u64::MAX,
        tenant: Some(tenant.to_string()),
        input_closure_digest: String::new(),
    })
}

/// `path_tenants.tenant_id` rows for one store path, sorted.
async fn junction_tenants(pool: &sqlx::PgPool, path: &str) -> Vec<uuid::Uuid> {
    let mut rows: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(path_hash(path))
            .fetch_all(pool)
            .await
            .expect("path_tenants query");
    rows.sort();
    rows
}

/// A `PutPath` whose stream is gated by the test: the metadata frame is
/// sent immediately (so the handler claims the `'uploading'`
/// placeholder), but NAR chunk + trailer wait until the test releases
/// them through the returned sender. Returns the frame sender, the
/// pending trailer, and the join handle for the RPC.
async fn put_path_gated(
    client: &StoreServiceClient<Channel>,
    info: ValidatedPathInfo,
    token: &str,
) -> (
    mpsc::Sender<PutPathRequest>,
    PutPathTrailer,
    tokio::task::JoinHandle<Result<bool, tonic::Status>>,
) {
    let mut raw: PathInfo = info.into();
    let trailer = PutPathTrailer {
        nar_hash: std::mem::take(&mut raw.nar_hash),
        nar_size: std::mem::take(&mut raw.nar_size),
    };
    let (tx, rx) = mpsc::channel(8);
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(raw),
            declared_nar_size: 0,
        })),
    })
    .await
    .expect("fresh channel");

    let mut req = tonic::Request::new(ReceiverStream::new(rx));
    req.metadata_mut().insert(
        rio_proto::ASSIGNMENT_TOKEN_HEADER,
        token.parse().expect("token is ASCII"),
    );
    let mut rpc_client = client.clone();
    let handle = tokio::spawn(async move {
        rpc_client
            .put_path(req)
            .await
            .map(|r| r.into_inner().created)
    });
    (tx, trailer, handle)
}

/// Block until the winner's `'uploading'` placeholder row exists —
/// the deterministic "winner has claimed" sync point.
async fn wait_for_placeholder(pool: &sqlx::PgPool, path: &str) {
    for _ in 0..100 {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM manifests WHERE store_path_hash = $1 AND status = 'uploading'",
        )
        .bind(path_hash(path))
        .fetch_one(pool)
        .await
        .expect("manifests query");
        if n == 1 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("winner never claimed the 'uploading' placeholder");
}

/// Block until the loser has actually entered the bounded-wait arm,
/// observable via the `outcome=waiting` entry counter. Replaces a
/// blind 300 ms sleep that could let the whole test resolve through
/// the idempotent-skip fast path under load — passing without ever
/// exercising the wait it exists to verify.
///
/// Works because `#[tokio::test]` is a current-thread runtime: the
/// spawned server task emits into the thread-local recorder (same
/// pattern as rio-gateway/tests/ssh_hardening.rs).
async fn wait_for_wait_arm(recorder: &CountingRecorder) {
    const KEY: &str = "rio_store_putpath_concurrent_wait_total{outcome=waiting}";
    for _ in 0..500 {
        if recorder.get(KEY) >= 1 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "loser never reached the concurrent-wait arm; counters seen: {:?}",
        recorder.all_keys()
    );
}

/// The live repro: winner holds the placeholder mid-stream; loser
/// arrives for the same path. The loser MUST wait for the winner and
/// resolve as an idempotent skip (`created: false`) — not surface
/// ABORTED. Both tenants get their `path_tenants` junction row
/// (`r[store.put.tenant-junction]` skip semantics).
// r[verify store.put.concurrent-wait]
#[tokio::test]
async fn concurrent_putpath_loser_waits_for_winner_then_skips() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let s = StoreSession::new_with_hmac(KEY.to_vec()).await?;
    let tenant_a = seed_tenant(&s.db.pool, "conc-winner").await;
    let tenant_b = seed_tenant(&s.db.pool, "conc-loser").await;

    let (nar, _) = make_nar(b"concurrent contents");
    let path = test_store_path("concurrent-wait");
    let info = make_path_info_for_nar(&path, &nar);

    // Winner: metadata only — placeholder claimed, upload "in flight".
    let (winner_tx, trailer, winner) = put_path_gated(
        &s.client,
        info.clone(),
        &token_for(tenant_a, vec![path.clone()]),
    )
    .await;
    wait_for_placeholder(&s.db.pool, &path).await;

    // Loser: full upload for the SAME path, different tenant.
    let mut loser_client = s.client.clone();
    let loser_info = info.clone();
    let loser_nar = nar.clone();
    let loser_token = token_for(tenant_b, vec![path.clone()]);
    let loser = tokio::spawn(async move {
        put_path_with_token(&mut loser_client, loser_info, loser_nar, &loser_token).await
    });

    // Only proceed once the loser is provably parked in the wait arm —
    // then let the winner finish.
    wait_for_wait_arm(&recorder).await;
    winner_tx
        .send(PutPathRequest {
            msg: Some(put_path_request::Msg::NarChunk(nar.clone())),
        })
        .await
        .expect("winner stream open");
    winner_tx
        .send(PutPathRequest {
            msg: Some(put_path_request::Msg::Trailer(trailer)),
        })
        .await
        .expect("winner stream open");
    drop(winner_tx);

    assert!(winner.await?.context("winner upload")?, "winner commits");
    let loser_created = loser
        .await?
        .context("loser must wait out the winner, not surface ABORTED")?;
    assert!(!loser_created, "loser resolves as idempotent skip");

    assert_eq!(
        junction_tenants(&s.db.pool, &path).await,
        {
            let mut t = vec![tenant_a, tenant_b];
            t.sort();
            t
        },
        "winner commits its junction row; waiting loser writes the skip row"
    );
    Ok(())
}

/// The wait is BOUNDED: a winner that never finishes (held placeholder,
/// fresh heartbeat) must not park the loser forever — after the
/// configured budget the original ABORTED surfaces so the client's own
/// retry logic stays in charge.
// r[verify store.put.concurrent-wait]
#[tokio::test]
async fn concurrent_putpath_wait_budget_bounded() -> TestResult {
    let s = StoreSession::build(|pool| {
        StoreServiceImpl::new(pool)
            .with_hmac_verifier(Arc::new(HmacVerifier::from_key(KEY.to_vec())))
            .with_concurrent_put_wait(std::time::Duration::from_millis(200))
    })
    .await?;
    let tenant_a = seed_tenant(&s.db.pool, "conc-stall-winner").await;
    let tenant_b = seed_tenant(&s.db.pool, "conc-stall-loser").await;

    let (nar, _) = make_nar(b"stalled contents");
    let path = test_store_path("concurrent-stall");
    let info = make_path_info_for_nar(&path, &nar);

    // Winner claims and then stalls forever (tx held open, no frames).
    let (_winner_tx, _trailer, _winner) = put_path_gated(
        &s.client,
        info.clone(),
        &token_for(tenant_a, vec![path.clone()]),
    )
    .await;
    wait_for_placeholder(&s.db.pool, &path).await;

    let mut loser_client = s.client.clone();
    let err = put_path_with_token(
        &mut loser_client,
        info,
        nar,
        &token_for(tenant_b, vec![path.clone()]),
    )
    .await
    .expect_err("budget exhausted must surface the original ABORTED");
    assert_eq!(err.code(), tonic::Code::Aborted, "status: {err:?}");
    assert!(
        err.message().contains(rio_proto::CONCURRENT_PUTPATH_MSG),
        "message names the contention: {}",
        err.message()
    );
    Ok(())
}

/// Winner dies mid-upload: its placeholder is reaped on the error path,
/// and the WAITING loser must claim the freed slot and complete its own
/// upload (`created: true`) instead of timing out.
// r[verify store.put.concurrent-wait]
#[tokio::test]
async fn concurrent_putpath_winner_abort_loser_takes_over() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let s = StoreSession::new_with_hmac(KEY.to_vec()).await?;
    let _tenant_a = seed_tenant(&s.db.pool, "conc-dead-winner").await;
    let tenant_b = seed_tenant(&s.db.pool, "conc-dead-loser").await;

    let (nar, _) = make_nar(b"takeover contents");
    let path = test_store_path("concurrent-takeover");
    let info = make_path_info_for_nar(&path, &nar);

    let (winner_tx, _trailer, winner) = put_path_gated(
        &s.client,
        info.clone(),
        &token_for(_tenant_a, vec![path.clone()]),
    )
    .await;
    wait_for_placeholder(&s.db.pool, &path).await;

    let mut loser_client = s.client.clone();
    let loser_info = info.clone();
    let loser_nar = nar.clone();
    let loser_token = token_for(tenant_b, vec![path.clone()]);
    let loser = tokio::spawn(async move {
        put_path_with_token(&mut loser_client, loser_info, loser_nar, &loser_token).await
    });
    // Only proceed once the loser is provably parked in the wait arm —
    // a takeover can only be exercised by a waiter that exists.
    wait_for_wait_arm(&recorder).await;

    // Winner dies: stream ends without trailer → handler error path →
    // abort_upload reaps the placeholder.
    drop(winner_tx);
    assert!(winner.await?.is_err(), "truncated winner upload fails");

    let loser_created = loser
        .await?
        .context("loser must take over the freed placeholder")?;
    assert!(loser_created, "loser performs the actual upload");
    assert_eq!(
        junction_tenants(&s.db.pool, &path).await,
        vec![tenant_b],
        "only the tenant that actually committed has a junction row"
    );
    Ok(())
}
