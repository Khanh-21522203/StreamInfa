#![cfg(not(feature = "s3"))]

mod common;

use axum::body::{to_bytes, Body};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::util::ServiceExt;

#[tokio::test]
async fn control_plane_auth_and_crud_round_trip() {
    let (app, _) = common::build_test_app(true);

    let missing_auth_req = Request::builder()
        .method("POST")
        .uri("/api/v1/streams")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"ingest_type":"rtmp"}"#))
        .unwrap();
    let missing_auth_resp = app.clone().oneshot(missing_auth_req).await.unwrap();
    assert_eq!(missing_auth_resp.status(), StatusCode::UNAUTHORIZED);

    let invalid_auth_req = Request::builder()
        .method("POST")
        .uri("/api/v1/streams")
        .header(CONTENT_TYPE, "application/json")
        .header(AUTHORIZATION, "Bearer at_invalid")
        .body(Body::from(r#"{"ingest_type":"rtmp"}"#))
        .unwrap();
    let invalid_auth_resp = app.clone().oneshot(invalid_auth_req).await.unwrap();
    assert_eq!(invalid_auth_resp.status(), StatusCode::FORBIDDEN);

    let create_req = Request::builder()
        .method("POST")
        .uri("/api/v1/streams")
        .header(CONTENT_TYPE, "application/json")
        .header(
            AUTHORIZATION,
            format!("Bearer {}", common::TEST_ADMIN_TOKEN),
        )
        .body(Body::from(
            r#"{"ingest_type":"rtmp","metadata":{"suite":"phase-e"}}"#,
        ))
        .unwrap();
    let create_resp = app.clone().oneshot(create_req).await.unwrap();
    assert!(
        matches!(create_resp.status(), StatusCode::OK | StatusCode::CREATED),
        "unexpected create status: {}",
        create_resp.status()
    );
    let create_body = to_bytes(create_resp.into_body(), usize::MAX).await.unwrap();
    let create_json: Value = serde_json::from_slice(&create_body).unwrap();
    let stream_id = create_json
        .get("stream_id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();
    assert_eq!(
        create_json.get("status").and_then(Value::as_str),
        Some("pending")
    );
    assert!(create_json
        .get("stream_key")
        .and_then(Value::as_str)
        .is_some());

    let list_req = Request::builder()
        .method("GET")
        .uri("/api/v1/streams?limit=10&offset=0")
        .header(
            AUTHORIZATION,
            format!("Bearer {}", common::TEST_ADMIN_TOKEN),
        )
        .body(Body::empty())
        .unwrap();
    let list_resp = app.clone().oneshot(list_req).await.unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_body = to_bytes(list_resp.into_body(), usize::MAX).await.unwrap();
    let list_json: Value = serde_json::from_slice(&list_body).unwrap();
    assert!(list_json
        .get("streams")
        .and_then(Value::as_array)
        .map(|streams| !streams.is_empty())
        .unwrap_or(false));

    let get_req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/streams/{stream_id}"))
        .header(
            AUTHORIZATION,
            format!("Bearer {}", common::TEST_ADMIN_TOKEN),
        )
        .body(Body::empty())
        .unwrap();
    let get_resp = app.clone().oneshot(get_req).await.unwrap();
    assert_eq!(get_resp.status(), StatusCode::OK);
    let get_body = to_bytes(get_resp.into_body(), usize::MAX).await.unwrap();
    let get_json: Value = serde_json::from_slice(&get_body).unwrap();
    assert_eq!(
        get_json.get("stream_id").and_then(Value::as_str),
        Some(stream_id.as_str())
    );

    let delete_req = Request::builder()
        .method("DELETE")
        .uri(format!("/api/v1/streams/{stream_id}"))
        .header(
            AUTHORIZATION,
            format!("Bearer {}", common::TEST_ADMIN_TOKEN),
        )
        .body(Body::empty())
        .unwrap();
    let delete_resp = app.clone().oneshot(delete_req).await.unwrap();
    assert_eq!(delete_resp.status(), StatusCode::OK);
    let delete_body = to_bytes(delete_resp.into_body(), usize::MAX).await.unwrap();
    let delete_json: Value = serde_json::from_slice(&delete_body).unwrap();
    assert_eq!(
        delete_json.get("status").and_then(Value::as_str),
        Some("deleted")
    );
}

#[tokio::test]
async fn health_and_metrics_endpoints_are_available() {
    let (app, _) = common::build_test_app(false);

    let health_req = Request::builder()
        .method("GET")
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let health_resp = app.clone().oneshot(health_req).await.unwrap();
    assert_eq!(health_resp.status(), StatusCode::OK);

    let metrics_req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let metrics_resp = app.clone().oneshot(metrics_req).await.unwrap();
    assert_eq!(metrics_resp.status(), StatusCode::OK);
    let _ = to_bytes(metrics_resp.into_body(), usize::MAX)
        .await
        .unwrap();
}
