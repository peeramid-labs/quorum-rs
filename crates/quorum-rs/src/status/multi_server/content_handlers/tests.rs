use super::*;

use crate::pipeline::{RECEIVED_ALL, STREAM, received_subject};
use axum::http::Request;
use futures::StreamExt as _;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;
use wiremock::matchers::{header as has_header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BOUNDARY: &str = "----quorumtestboundary";
const TOKEN: &str = "operator-bearer";

fn multipart(filename: &str, bytes: &[u8], visibility: Option<&str>) -> Vec<u8> {
    let mut body = Vec::new();
    if let Some(visibility) = visibility {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"visibility\"\r\n\r\n{visibility}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// Bytes that sniff as an mp4.
fn fake_mp4(filler: &str) -> Vec<u8> {
    let mut bytes = b"\x00\x00\x00\x20ftypisom\x00\x00\x02\x00".to_vec();
    bytes.extend_from_slice(filler.as_bytes());
    bytes
}

async fn broker() -> Option<async_nats::jetstream::Context> {
    let url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".into());
    async_nats::connect(&url)
        .await
        .ok()
        .map(async_nats::jetstream::new)
}

/// The routes over `node`, staging for a fresh owner on `js`.
fn configured(js: async_nats::jetstream::Context, node: &str, owner: &str) -> ContentUploads {
    ContentUploads {
        js,
        owner: owner.to_string(),
        node: NodeFiles {
            http: reqwest::Client::new(),
            base: node.to_string(),
            token: TOKEN.to_string(),
        },
        max_upload_bytes: 4096,
        spool_dir: None,
        uploaded_by: "agent-7".to_string(),
    }
}

async fn send(content: Option<ContentUploads>, request: Request<Body>) -> Response {
    super::super::build_router(MultiAppState::for_content(content))
        .oneshot(request)
        .await
        .expect("a response")
}

fn upload_request(body: Vec<u8>) -> Request<Body> {
    Request::post("/api/content")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .expect("request")
}

async fn body_bytes(response: Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec()
}

async fn body_json(response: Response) -> serde_json::Value {
    serde_json::from_slice(&body_bytes(response).await).unwrap_or_default()
}

fn digest_of(bytes: &[u8]) -> String {
    crate::files::sha256_hex_bytes(bytes)
}

#[tokio::test]
async fn an_upload_is_staged_and_announced_for_the_operator() {
    let Some(js) = broker().await else {
        eprintln!("no broker; skipping");
        return;
    };
    crate::pipeline::ensure(&js, 16 * 1024 * 1024)
        .await
        .expect("the pipeline");
    let owner = format!("op_dash{}", uuid::Uuid::new_v4().simple());
    let stream = js.get_stream(STREAM).await.expect("stream");
    let announced = stream
        .create_consumer(async_nats::jetstream::consumer::pull::Config {
            filter_subject: received_subject(&owner),
            ..Default::default()
        })
        .await
        .expect("consumer");
    assert_ne!(received_subject(&owner), RECEIVED_ALL);

    let video = fake_mp4("a talk");
    let response = send(
        Some(configured(js.clone(), "http://127.0.0.1:9", &owner)),
        upload_request(multipart("talk.mp4", &video, Some("public"))),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let staged = body_json(response).await;
    assert_eq!(staged["state"], "staged");
    assert_eq!(staged["digest"], digest_of(&video));
    assert_eq!(staged["mime"], "video/mp4");

    let mut messages = announced
        .fetch()
        .max_messages(1)
        .messages()
        .await
        .expect("fetch");
    let message = messages.next().await.expect("announced").expect("message");
    message.ack().await.expect("ack");
    let announcement: crate::pipeline::Staged =
        serde_json::from_slice(&message.payload).expect("a staged upload");
    assert_eq!(announcement.upload_id, staged["upload_id"]);
    assert_eq!(announcement.meta.uploaded_by, "agent-7");
    crate::pipeline::discard(&js, &announcement)
        .await
        .expect("discard");
}

#[tokio::test]
async fn reads_status_usage_and_delete_are_the_nodes_answers_asked_as_the_operator() {
    let Some(js) = broker().await else {
        eprintln!("no broker; skipping");
        return;
    };
    let node = MockServer::start().await;
    let digest = digest_of(b"stored");
    Mock::given(method("GET"))
        .and(path(format!("/api/content/{digest}")))
        .and(has_header("authorization", format!("Bearer {TOKEN}")))
        .and(has_header("range", "bytes=0-2"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 0-2/6")
                .insert_header("content-type", "video/mp4")
                .set_body_bytes(b"sto".to_vec()),
        )
        .mount(&node)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/content/{digest}/status")))
        .and(has_header("authorization", format!("Bearer {TOKEN}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"state": "ready", "hls": "ready"})),
        )
        .mount(&node)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/operators/storage"))
        .and(has_header("authorization", format!("Bearer {TOKEN}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"used_bytes": 6, "quota_bytes": 100})),
        )
        .mount(&node)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/api/content/{digest}")))
        .and(has_header("authorization", format!("Bearer {TOKEN}")))
        .respond_with(ResponseTemplate::new(204))
        .mount(&node)
        .await;
    let content = || Some(configured(js.clone(), &node.uri(), "op_x"));

    let ranged = Request::get(format!("/api/content/{digest}"))
        .header(header::RANGE, "bytes=0-2")
        .body(Body::empty())
        .expect("request");
    let response = send(content(), ranged).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 0-2/6");
    assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
    assert_eq!(body_bytes(response).await, b"sto");

    let get = |uri: String| Request::get(uri).body(Body::empty()).expect("request");
    let response = send(content(), get(format!("/api/content/{digest}/status"))).await;
    assert_eq!(body_json(response).await["hls"], "ready");
    let response = send(content(), get("/api/content/usage".into())).await;
    assert_eq!(body_json(response).await["quota_bytes"], 100);
    let delete = Request::delete(format!("/api/content/{digest}"))
        .body(Body::empty())
        .expect("request");
    assert_eq!(
        send(content(), delete).await.status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn a_node_that_does_not_answer_is_a_bad_gateway_and_its_refusals_pass_through() {
    let Some(js) = broker().await else {
        eprintln!("no broker; skipping");
        return;
    };
    let digest = digest_of(b"x");
    let status = |node: &str| {
        (
            Some(configured(js.clone(), node, "op_x")),
            Request::get(format!("/api/content/{digest}/status"))
                .body(Body::empty())
                .expect("request"),
        )
    };
    let (content, request) = status("http://127.0.0.1:9");
    assert_eq!(
        send(content, request).await.status(),
        StatusCode::BAD_GATEWAY
    );

    let node = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "no such file"})),
        )
        .mount(&node)
        .await;
    let (content, request) = status(&node.uri());
    let response = send(content, request).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(response).await["error"], "no such file");
}

#[tokio::test]
async fn a_digest_shaped_probe_never_reaches_the_node() {
    let Some(js) = broker().await else {
        eprintln!("no broker; skipping");
        return;
    };
    let node = MockServer::start().await;
    for probe in ["notes.abc", "ABCDEF", &"a".repeat(63)] {
        let request = Request::get(format!("/api/content/{probe}"))
            .body(Body::empty())
            .expect("request");
        let response = send(Some(configured(js.clone(), &node.uri(), "op_x")), request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{probe}");
    }
    assert!(
        node.received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
}

#[tokio::test]
async fn an_upload_is_refused_by_type_form_or_declared_size_before_anything_is_staged() {
    let Some(js) = broker().await else {
        eprintln!("no broker; skipping");
        return;
    };
    let content = || Some(configured(js.clone(), "http://127.0.0.1:9", "op_x"));
    let response = send(
        content(),
        upload_request(multipart("run.sh", b"#!/bin/sh\necho hi\n", Some("public"))),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let response = send(
        content(),
        upload_request(multipart("talk.mp4", &fake_mp4("x"), None)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "no visibility");

    let mut huge = upload_request(multipart("talk.mp4", &fake_mp4("x"), Some("public")));
    huge.headers_mut()
        .insert(header::CONTENT_LENGTH, "999999999".parse().expect("header"));
    assert_eq!(
        send(content(), huge).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn an_agent_with_no_files_configured_says_so() {
    let response = send(
        None,
        Request::get("/api/content/usage")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        body_json(response).await["error"]
            .as_str()
            .unwrap_or_default()
            .contains("NSED_FILES_OWNER")
    );
}
