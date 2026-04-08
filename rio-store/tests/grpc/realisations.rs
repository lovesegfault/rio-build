//! gRPC-level tests for RegisterRealisation, QueryRealisation.
//!
//! The underlying `realisations::*` are unit-tested directly; these
//! tests cover the gRPC wrapper's input validation branches (hash
//! length, empty output_name, missing fields).

use super::*;
use rio_proto::types::{QueryRealisationRequest, Realisation, RegisterRealisationRequest};

// ---------------------------------------------------------------------------
// RegisterRealisation — validation branches
// ---------------------------------------------------------------------------

fn valid_realisation() -> Realisation {
    Realisation {
        drv_hash: vec![0xAA; 32],
        output_name: "out".into(),
        output_path: test_store_path("ca-output"),
        output_hash: vec![0xBB; 32],
        signatures: vec![],
    }
}

#[tokio::test]
async fn register_realisation_missing_field_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let err = s
        .client
        .register_realisation(RegisterRealisationRequest { realisation: None })
        .await
        .expect_err("None realisation → invalid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("required"), "msg: {}", err.message());

    Ok(())
}

#[tokio::test]
async fn register_realisation_bad_drv_hash_length_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let mut r = valid_realisation();
    r.drv_hash = vec![0xAA; 16]; // wrong length

    let err = s
        .client
        .register_realisation(RegisterRealisationRequest {
            realisation: Some(r),
        })
        .await
        .expect_err("short drv_hash → invalid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn register_realisation_bad_output_hash_length_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let mut r = valid_realisation();
    r.output_hash = vec![0xBB; 16];

    let err = s
        .client
        .register_realisation(RegisterRealisationRequest {
            realisation: Some(r),
        })
        .await
        .expect_err("short output_hash → invalid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn register_realisation_empty_output_name_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let mut r = valid_realisation();
    r.output_name = String::new();

    let err = s
        .client
        .register_realisation(RegisterRealisationRequest {
            realisation: Some(r),
        })
        .await
        .expect_err("empty output_name → invalid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn register_realisation_bad_output_path_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let mut r = valid_realisation();
    r.output_path = "not-a-store-path".into();

    let err = s
        .client
        .register_realisation(RegisterRealisationRequest {
            realisation: Some(r),
        })
        .await
        .expect_err("invalid output_path → invalid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    Ok(())
}

/// End-to-end: register → query roundtrip through gRPC.
#[tokio::test]
async fn register_then_query_realisation_roundtrip() -> TestResult {
    let mut s = StoreSession::new().await?;

    let r = valid_realisation();
    s.client
        .register_realisation(RegisterRealisationRequest {
            realisation: Some(r.clone()),
        })
        .await?;

    let got = s
        .client
        .query_realisation(QueryRealisationRequest {
            drv_hash: r.drv_hash.clone(),
            output_name: r.output_name.clone(),
        })
        .await?
        .into_inner();
    assert_eq!(got.output_path, r.output_path);
    assert_eq!(got.output_hash, r.output_hash);

    Ok(())
}

// ---------------------------------------------------------------------------
// QueryRealisation — validation branches
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_realisation_bad_drv_hash_length_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let err = s
        .client
        .query_realisation(QueryRealisationRequest {
            drv_hash: vec![0xAA; 16],
            output_name: "out".into(),
        })
        .await
        .expect_err("short drv_hash → invalid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn query_realisation_empty_output_name_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let err = s
        .client
        .query_realisation(QueryRealisationRequest {
            drv_hash: vec![0xAA; 32],
            output_name: String::new(),
        })
        .await
        .expect_err("empty output_name → invalid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn query_realisation_not_found() -> TestResult {
    let mut s = StoreSession::new().await?;

    let err = s
        .client
        .query_realisation(QueryRealisationRequest {
            drv_hash: vec![0xEE; 32], // never registered
            output_name: "out".into(),
        })
        .await
        .expect_err("unknown → not_found");
    assert_eq!(err.code(), tonic::Code::NotFound);

    Ok(())
}
