//! Discovery wire contracts and inactive installation regressions.
#![cfg(test)]

use super::*;
use axum::{Router, body::Body, http::Request, response::IntoResponse, routing::get};
use parking_lot::Mutex;
use std::sync::Arc;
use tower::ServiceExt;

struct Stub {
    base: String,
    requests: Arc<Mutex<Vec<(String, axum::http::HeaderMap)>>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Stub {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn stub(responses: Vec<(u16, Vec<(&'static str, &'static str)>, String)>) -> Stub {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let responses = Arc::new(Mutex::new(std::collections::VecDeque::from(responses)));
    let router = Router::new().route(
        "/search/repositories",
        get(move |request: Request<Body>| {
            let recorded = recorded.clone();
            let responses = responses.clone();
            async move {
                recorded
                    .lock()
                    .push((request.uri().to_string(), request.headers().clone()));
                let (status, headers, body) = responses.lock().pop_front().expect("unexpected upstream request");
                let mut response = (StatusCode::from_u16(status).unwrap(), body).into_response();
                for (name, value) in headers {
                    response.headers_mut().insert(name, value.parse().unwrap());
                }
                response
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Stub { base, requests, server }
}
async fn request(base: &str, query: &str) -> (StatusCode, String) {
    let base = base.to_string();
    let app = Router::new().route(
        "/discover",
        get(move |Query(params): Query<DiscoverQuery>| {
            let base = base.clone();
            async move { discover_with(params, None, &base).await }
        }),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/discover{query}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}
fn item(name: &str) -> serde_json::Value {
    serde_json::json!({"name":name,"html_url":format!("https://github.com/example/{name}")})
}

#[tokio::test]
async fn r1a_discovery_success_preserves_wire_bytes_and_headers() {
    let stub = stub(vec![(200, vec![], r#"{"items":[{"name":"first","html_url":"https://github.com/example/first","description":"fixture","stargazers_count":42,"language":"Rust","owner":{"login":"prx_user"},"license":{"spdx_id":"MIT"}},{"name":"second","html_url":"https://github.com/example/second","stargazers_count":0,"license":null}]}"#.into())]).await;
    let (status, body) = request(&stub.base, "?source=GitHub&query=agent%20skill").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        r#"{"results":[{"name":"first","url":"https://github.com/example/first","description":"fixture","stars":42,"language":"Rust","source":"GitHub","owner":"prx_user","has_license":true},{"name":"second","url":"https://github.com/example/second","description":"","stars":0,"language":null,"source":"GitHub","owner":"unknown","has_license":false}]}"#
    );
    let requests = stub.requests.lock();
    assert_eq!(requests.len(), 1);
    let (path, headers) = requests.first().unwrap();
    assert_eq!(
        path,
        "/search/repositories?q=agent+skill&sort=stars&order=desc&per_page=30"
    );
    assert_eq!(
        headers.get("accept").unwrap().to_str().unwrap(),
        "application/vnd.github+json"
    );
    let expected_agent = ["OpenPRX-Skill", "Forge/0.1"].concat();
    assert_eq!(
        headers.get("user-agent").unwrap().to_str().unwrap(),
        expected_agent.as_str()
    );
    assert!(!headers.contains_key("authorization"));
}
#[tokio::test]
async fn r1b_default_queries_deduplicate_keep_first_in_order() {
    let stub = stub(vec![
        (200, vec![], serde_json::json!({"items":[item("first"),item("shared")]}).to_string()),
        (200, vec![], serde_json::json!({"items":[{"name":"duplicate","html_url":"https://github.com/example/shared"},item("last")]}).to_string()),
    ]).await;
    let (status, body) = request(&stub.base, "").await;
    assert_eq!(status, StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    let names: Vec<_> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["first", "shared", "last"]);
    let paths: Vec<_> = stub.requests.lock().iter().map(|v| v.0.clone()).collect();
    assert_eq!(
        paths,
        [
            "/search/repositories?q=openprx+skill&sort=stars&order=desc&per_page=30",
            "/search/repositories?q=ai+agent+skill&sort=stars&order=desc&per_page=30"
        ]
    );
}
#[tokio::test]
async fn r1c_unsupported_and_unavailable_sources_never_contact_upstream() {
    // A restored fallback must receive successful responses, so the regression
    // observes the wrong 200 response and upstream hits rather than a stub fault.
    let stub = stub(vec![
        (200, vec![], "{\"items\":[]}".into()),
        (200, vec![], "{\"items\":[]}".into()),
    ])
    .await;
    let (status, body) = request(&stub.base, "?source=gitlab").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"code":"unsupported_source","error":"Unsupported skill discovery source 'gitlab'"})
    );
    for source in ["clawhub", "ClawHub", "huggingface", "HF"] {
        assert_eq!(
            request(&stub.base, &format!("?source={source}")).await,
            (StatusCode::OK, "{\"results\":[]}".into())
        );
    }
    assert!(stub.requests.lock().is_empty());
}
async fn error_case(
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: &str,
    expected_status: StatusCode,
    code: &str,
    error: &str,
) {
    let stub = stub(vec![(status, headers, body.into())]).await;
    assert_error(
        request(&stub.base, "?query=fixture").await,
        expected_status,
        code,
        error,
    );
    assert_eq!(stub.requests.lock().len(), 1);
}
fn assert_error((status, body): (StatusCode, String), expected: StatusCode, code: &str, error: &str) {
    assert_eq!(status, expected);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"code":code,"error":error})
    );
}
#[tokio::test]
async fn r7_rate_limit_remaining_zero() {
    error_case(
        403,
        vec![("x-ratelimit-remaining", "0")],
        "private upstream body",
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "GitHub search rate limit reached; try again later",
    )
    .await;
}
#[tokio::test]
async fn r7_rate_limit_retry_after() {
    error_case(
        403,
        vec![("retry-after", "10")],
        "private upstream body",
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "GitHub search rate limit reached; try again later",
    )
    .await;
}
#[tokio::test]
async fn r7_rate_limit_http_429() {
    error_case(
        429,
        vec![],
        "private upstream body",
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "GitHub search rate limit reached; try again later",
    )
    .await;
}
#[tokio::test]
async fn r7_plain_forbidden_is_upstream_error() {
    error_case(
        403,
        vec![],
        "private upstream body",
        StatusCode::BAD_GATEWAY,
        "upstream_error",
        "GitHub search failed: HTTP 403",
    )
    .await;
}
#[tokio::test]
async fn r7_transport_error_is_safe() {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    assert_error(
        request(&base, "?query=fixture").await,
        StatusCode::BAD_GATEWAY,
        "upstream_error",
        "GitHub search failed: request failed",
    );
}
#[tokio::test]
async fn r7_non_json_is_invalid_response() {
    error_case(
        200,
        vec![],
        "not JSON",
        StatusCode::BAD_GATEWAY,
        "upstream_error",
        "GitHub search failed: invalid response",
    )
    .await;
}
#[tokio::test]
async fn r7_missing_items_is_invalid_response() {
    error_case(
        200,
        vec![],
        r#"{"total_count":0}"#,
        StatusCode::BAD_GATEWAY,
        "upstream_error",
        "GitHub search failed: invalid response",
    )
    .await;
}
#[tokio::test]
async fn r7_non_array_items_is_invalid_response() {
    error_case(
        200,
        vec![],
        r#"{"items":{}}"#,
        StatusCode::BAD_GATEWAY,
        "upstream_error",
        "GitHub search failed: invalid response",
    )
    .await;
}
#[tokio::test]
async fn r7_later_query_failure_discards_partial_results() {
    let stub = stub(vec![
        (200, vec![], serde_json::json!({"items":[item("first")]}).to_string()),
        (500, vec![], "private upstream body".into()),
    ])
    .await;
    assert_error(
        request(&stub.base, "").await,
        StatusCode::BAD_GATEWAY,
        "upstream_error",
        "GitHub search failed: HTTP 500",
    );
    assert_eq!(stub.requests.lock().len(), 2);
}
#[tokio::test]
async fn r7_valid_empty_and_skipped_items_remain_successful() {
    for body in [r#"{"items":[]}"#, r#"{"items":[{}]}"#] {
        let stub = stub(vec![(200, vec![], body.into())]).await;
        assert_eq!(
            request(&stub.base, "?query=fixture").await,
            (StatusCode::OK, "{\"results\":[]}".into())
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn r8e_gateway_install_inventory_and_toggle_use_disabled_state() {
    let temp = tempfile::tempdir().unwrap();
    let config = crate::config::Config {
        workspace_dir: temp.path().to_path_buf(),
        ..Default::default()
    };
    let state = super::super::test_support::app_state(config);
    let Json(response) = install_skill_with(state.clone(),InstallRequest {url:"https://github.com/example/quarantine".into(),name:"quarantine".into()}, |_, staging| async move {
        std::fs::create_dir_all(&staging)?;
        std::fs::write(staging.join("SKILL.toml"),"[skill]\nname='quarantine'\ndescription='fixture'\n[[tools]]\nname='run'\ndescription='fixture tool'\nkind='shell'\ncommand='sh run.sh'\n")?;
        std::fs::write(staging.join("run.sh"),"exit 0\n")?;
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        Ok(std::process::Output {status: std::process::ExitStatus::from_raw(0), stdout:vec![],stderr:vec![]})
    }).await.unwrap();
    assert_eq!(Json(&response).into_response().status(), StatusCode::OK);
    assert_eq!(response.status, "ok");
    let installed = response.skill.unwrap();
    assert!(!installed.enabled, "gateway install must return disabled state");
    assert_eq!(installed.description, "fixture");
    let Json(inventory) = get_skills(State(state.clone())).await;
    assert!(
        inventory
            .skills
            .iter()
            .any(|skill| skill.name == "quarantine" && !skill.enabled)
    );
    let Json(toggled) = toggle_skill(State(state), Path("quarantine".into())).await.unwrap();
    assert_eq!(toggled["enabled"], true);
}
