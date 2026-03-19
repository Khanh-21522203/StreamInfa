#![cfg(not(feature = "s3"))]

mod common;

use axum::body::{to_bytes, Body};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::util::ServiceExt;
use uuid::Uuid;

use streaminfa::control::state::StreamState;
use streaminfa::core::types::StreamId;
use streaminfa::storage::MediaStore;

#[tokio::test]
async fn create_ready_playback_delete_flow() {
    let (app, state) = common::build_test_app(true);

    let create_req = Request::builder()
        .method("POST")
        .uri("/api/v1/streams")
        .header(CONTENT_TYPE, "application/json")
        .header(
            AUTHORIZATION,
            format!("Bearer {}", common::TEST_ADMIN_TOKEN),
        )
        .body(Body::from(r#"{"ingest_type":"upload"}"#))
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

    let stream_uuid = Uuid::parse_str(&stream_id).unwrap();
    let typed_stream_id = StreamId::from_uuid(stream_uuid);

    state
        .state_manager
        .transition(typed_stream_id, StreamState::Processing)
        .await
        .unwrap();
    state
        .state_manager
        .transition(typed_stream_id, StreamState::Ready)
        .await
        .unwrap();

    let manifest_path = format!("{stream_id}/master.m3u8");
    let manifest_body = "#EXTM3U\n#EXT-X-VERSION:7\n";
    state
        .store
        .put_manifest(&manifest_path, manifest_body)
        .await
        .unwrap();

    let playback_req = Request::builder()
        .method("GET")
        .uri(format!("/streams/{stream_id}/master.m3u8"))
        .body(Body::empty())
        .unwrap();
    let playback_resp = app.clone().oneshot(playback_req).await.unwrap();
    assert_eq!(playback_resp.status(), StatusCode::OK);
    let content_type = playback_resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_type.starts_with("application/vnd.apple.mpegurl"));
    let playback_body = to_bytes(playback_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(playback_body, manifest_body);

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

    let playback_after_delete_req = Request::builder()
        .method("GET")
        .uri(format!("/streams/{stream_id}/master.m3u8"))
        .body(Body::empty())
        .unwrap();
    let playback_after_delete_resp = app
        .clone()
        .oneshot(playback_after_delete_req)
        .await
        .unwrap();
    assert_eq!(playback_after_delete_resp.status(), StatusCode::NOT_FOUND);
}
