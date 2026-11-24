//! 控制平面闭环：真实路由、权限、SQLite、操作恢复，不访问真实上游。

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use fakemodelproxy::{
    application::{
        access::{AccessKey, Scope},
        management::Management,
        operation_service::{Operation, OperationRequest},
    },
    backend::Registry,
    config::Config,
    records::RecordLog,
    routing::Router,
    state::AppState,
    store::{BackendRecord, Store},
    tasks::TaskRegistry,
    usage::UsageLedger,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, RwLock},
};
use tower::ServiceExt;

/// 每次使用独立数据库，可传磁盘路径验证重启后的未知状态。
async fn state(path: &Path) -> AppState {
    let store = Store::open(path).await.unwrap();
    let usage = Arc::new(UsageLedger::new(store.clone()));
    let config = Config {
        admin_key: "administrator-key".into(),
        access_keys: vec![
            AccessKey {
                id: "reader".into(),
                key: "read-only-access-key".into(),
                scopes: [Scope::Read].into(),
            },
            AccessKey {
                id: "operator".into(),
                key: "operations-access-key".into(),
                scopes: [Scope::Read, Scope::Operation].into(),
            },
        ],
        ..Default::default()
    };
    let state = AppState::new(
        Arc::new(config),
        store,
        Arc::new(Registry::new()),
        Arc::new(RecordLog::new(100)),
        Arc::new(RwLock::new(TaskRegistry::new())),
        Arc::new(Router::new()),
        usage,
    );
    state.reload_backends().await.unwrap();
    state
}

/// 通过真实 Axum 传输检查响应，不直接绕过 HTTP 身份验证。
async fn request(
    state: &AppState,
    key: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let response = fakemodelproxy::build_router(state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("x-admin-key", key)
                .header("content-type", "application/json")
                .body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 10_000_000).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"body":String::from_utf8_lossy(&bytes)})),
    )
}

/// 向运行状态添加无网络固定目录服务。
async fn add_service(state: &AppState, id: &str) {
    state
        .config_service()
        .save_backend(&BackendRecord {
            id: id.into(),
            name: id.into(),
            base_url: "https://example.invalid/v1".into(),
            models: "model".into(),
            ..Default::default()
        })
        .await
        .unwrap();
}

/// 轮询本地记录到所需状态，限时约束避免测试无限等待。
async fn wait_status(state: &AppState, id: &str, statuses: &[&str]) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (status, value) = request(
                state,
                "administrator-key",
                "GET",
                &format!("/api/v2/operations/{id}"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            if statuses.contains(&value["status"].as_str().unwrap()) {
                return value;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

/// 只读用户能拟定计划，但不能 apply、写秘密、执行动作或绕过 v1。
#[tokio::test]
async fn permissions_apply_to_use_cases_and_legacy_paths() {
    let state = state(Path::new(":memory:")).await;
    let (_, plugins) = request(
        &state,
        "read-only-access-key",
        "GET",
        "/api/v2/plugins",
        None,
    )
    .await;
    assert_eq!(plugins["plugins"].as_array().unwrap().len(), 9);
    let (status, _) = request(&state, "", "GET", "/api/v2/config", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, plan) = request(
        &state,
        "read-only-access-key",
        "POST",
        "/api/v2/config/plans",
        Some(json!({"expected_revision":0,"change":{}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    for (path, input) in [
        (
            format!(
                "/api/v2/config/plans/{}/apply",
                plan["id"].as_str().unwrap()
            ),
            json!({"idempotency_key":"apply"}),
        ),
        ("/api/v2/secrets".into(), json!({"value":"private"})),
        (
            "/api/v2/operations".into(),
            json!({"service_id":"any","action":"models.sync","idempotency_key":"run"}),
        ),
    ] {
        assert_eq!(
            request(&state, "read-only-access-key", "POST", &path, Some(input))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        request(
            &state,
            "read-only-access-key",
            "GET",
            "/api/config/export",
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let actor = fakemodelproxy::application::access::Principal::authenticate(
        &state.config,
        Some("read-only-access-key"),
    )
    .unwrap();
    assert_eq!(
        Management::new(&state, &actor)
            .write_secret("hidden")
            .await
            .unwrap_err()
            .code(),
        "forbidden"
    );
    assert_eq!(
        request(
            &state,
            "operations-access-key",
            "POST",
            "/api/v2/operations",
            Some(json!({
                "service_id":"q","action":"auth.start","idempotency_key":"auth"
            }))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

/// 具名写入只返回新引用；计划/apply/读取闭环不泄露秘密且拒绝旧 revision。
#[tokio::test]
async fn secret_reference_plan_apply_and_conflict_are_machine_readable() {
    let state = state(Path::new(":memory:")).await;
    let (status, secret) = request(
        &state,
        "administrator-key",
        "POST",
        "/api/v2/secrets",
        Some(json!({"value":"never-in-machine-output"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!secret.to_string().contains("never-in-machine-output"));
    let input = json!({"expected_revision":0,"change":{"services":[{
        "api_version":"fmp/v2","kind":"Service","id":"one","name":"One",
        "plugin_id":"openai_compat","plugin_config_version":1,"enabled":true,
        "config":{"base_url":"https://example.invalid/v1","models":["m"]},
        "credentials":{"api_key":{"secret_ref":secret["secret_ref"]}}
    }]}});
    let (status, plan) = request(
        &state,
        "administrator-key",
        "POST",
        "/api/v2/config/plans",
        Some(input.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    let path = format!(
        "/api/v2/config/plans/{}/apply",
        plan["id"].as_str().unwrap()
    );
    for _ in 0..2 {
        let (status, applied) = request(
            &state,
            "administrator-key",
            "POST",
            &path,
            Some(json!({"idempotency_key":"same"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(applied["active_revision"], 1);
    }
    let (status, failed) = request(
        &state,
        "administrator-key",
        "POST",
        "/api/v2/config/plans",
        Some(input),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(failed["error"]["code"], "revision_conflict");
    let (_, current) = request(
        &state,
        "read-only-access-key",
        "GET",
        "/api/v2/config",
        None,
    )
    .await;
    assert!(!current.to_string().contains("never-in-machine-output"));
    assert_eq!(
        state
            .store
            .get_backend("one")
            .await
            .unwrap()
            .unwrap()
            .api_key,
        "never-in-machine-output"
    );
}

/// 操作重复提交返回同一 ID，输入改变拒绝，服务被删后仍可读旧结果。
#[tokio::test]
async fn operations_are_idempotent_and_survive_service_removal() {
    let state = state(Path::new(":memory:")).await;
    add_service(&state, "one").await;
    let input = json!({"service_id":"one","action":"models.sync","idempotency_key":"sync-once"});
    let (status, first) = request(
        &state,
        "administrator-key",
        "POST",
        "/api/v2/operations",
        Some(input.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let id = first["operation_id"].as_str().unwrap();
    assert_eq!(
        wait_status(&state, id, &["succeeded", "failed"]).await["status"],
        "succeeded"
    );
    state.config_service().delete_backend("one").await.unwrap();
    let (_, repeated) = request(
        &state,
        "administrator-key",
        "POST",
        "/api/v2/operations",
        Some(input),
    )
    .await;
    assert_eq!(repeated["operation_id"], id);
    assert_eq!(repeated["status"], "succeeded");
    let (status, rejected) = request(
        &state,
        "administrator-key",
        "POST",
        "/api/v2/operations",
        Some(json!({
            "service_id":"other","action":"models.sync","idempotency_key":"sync-once"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(rejected["error"]["code"], "idempotency_conflict");
    assert_eq!(
        request(
            &state,
            "read-only-access-key",
            "GET",
            &format!("/api/v2/operations/{id}"),
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

/// 重启遇到进行中动作改为 unknown；同键重试不创建后台任务。
#[tokio::test]
async fn restart_marks_inflight_unknown_without_replay() {
    use sha2::{Digest, Sha256};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("state.db");
    let before = state(&path).await;
    let request = OperationRequest {
        service_id: "deleted".into(),
        action: "models.sync".into(),
        input: json!({}),
        idempotency_key: "crashed".into(),
    };
    let digest = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&json!({
                "service_id":request.service_id,"action":request.action,"input":request.input
            }))
            .unwrap()
        )
    );
    let operation: Operation = serde_json::from_value(json!({
        "operation_id":"interrupted","actor":"admin","service_id":"deleted","plugin_id":"openai_compat",
        "action":"models.sync","status":"running","created_at":1,"deadline":9999999999_u64,
        "result":null,"error":null,"next_action":null
    })).unwrap();
    before
        .store
        .create_operation(&operation, "crashed", &digest)
        .await
        .unwrap();
    drop(before);
    let after = state(&path).await;
    let actor = fakemodelproxy::application::access::Principal::authenticate(
        &after.config,
        Some("administrator-key"),
    )
    .unwrap();
    let op = fakemodelproxy::application::operation_service::OperationService::new(&after, &actor)
        .start(request)
        .await
        .unwrap();
    assert_eq!(op.operation_id, "interrupted");
    assert_eq!(op.status, "unknown");
    assert_eq!(op.error.unwrap()["retryable"], false);
}

/// Qoder 启动仅生成本地 PKCE；操作可交接给用户和取消，未发起上游轮询。
#[tokio::test]
async fn authorization_operation_is_trackable_and_cancellable() {
    let state = state(Path::new(":memory:")).await;
    state
        .config_service()
        .save_backend(&BackendRecord {
            id: "q".into(),
            name: "Qoder".into(),
            kind: "qoder".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (_, operation) = request(
        &state,
        "administrator-key",
        "POST",
        "/api/v2/operations",
        Some(json!({
            "service_id":"q","action":"auth.start","idempotency_key":"login"
        })),
    )
    .await;
    let id = operation["operation_id"].as_str().unwrap();
    let waiting = wait_status(&state, id, &["waiting_user"]).await;
    assert_eq!(waiting["next_action"]["type"], "open_url");
    assert!(
        waiting["next_action"]["url"]
            .as_str()
            .unwrap()
            .starts_with("https://qoder.com")
    );
    assert!(waiting.get("session").is_none());
    let (status, cancelled) = request(
        &state,
        "administrator-key",
        "POST",
        &format!("/api/v2/operations/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["status"], "cancelled");
    assert!(state.store.list_accounts().await.unwrap().is_empty());
}

/// 未设置管理密钥的非回环监听拒绝 v1/v2，调用密钥不能默认为独立管理权限。
#[tokio::test]
async fn remote_management_requires_authentication() {
    let mut state = state(Path::new(":memory:")).await;
    state.config = Arc::new(Config {
        listen: "0.0.0.0:8317".into(),
        api_key: "inference-key".into(),
        ..Default::default()
    });
    assert_eq!(
        request(&state, "", "GET", "/api/v2/config", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(&state, "", "GET", "/api/backends", None).await.0,
        StatusCode::UNAUTHORIZED
    );
}

/// 路由解释不隐式访问目录接口；类型错误返回 JSON 错误码而非提交内容。
#[tokio::test]
async fn explain_is_local_and_schema_errors_are_structured() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let upstream = axum::Router::new().fallback(move || {
        let count = count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            axum::Json(json!({"data":[]}))
        }
    });
    let server = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let state = state(Path::new(":memory:")).await;
    state
        .config_service()
        .save_backend(&BackendRecord {
            id: "gateway".into(),
            name: "Gateway".into(),
            base_url: base,
            ..Default::default()
        })
        .await
        .unwrap();
    let (status, value) = request(
        &state,
        "read-only-access-key",
        "POST",
        "/api/v2/routes/explain",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["source"], "cached_snapshot");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let (status, error) = request(
        &state,
        "read-only-access-key",
        "POST",
        "/api/v2/routes/explain",
        Some(json!({"input_tokens":"private-input"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["error"]["code"], "schema_invalid");
    assert!(!error.to_string().contains("private-input"));
    server.abort();
}

/// 预设是公开目录数据；批量错误回滚，更新不重写已经配置的服务。
#[tokio::test]
async fn custom_presets_are_validated_atomically_and_do_not_change_services() {
    let state = state(Path::new(":memory:")).await;
    add_service(&state, "one").await;
    let preset = json!({
        "api_version":"fmp/preset/v1","id":"custom-gateway","name":"自定义网关",
        "plugin_id":"openai_compat","plugin_config_version":1,
        "config":{"base_url":"https://example.invalid/v1","models":["m"]},
        "policy":{"priority":25,"cost_tier":"unknown"},"docs":"https://example.invalid/docs"
    });
    let mut invalid = preset.clone();
    invalid["id"] = json!("invalid");
    invalid["plugin_id"] = json!("manifest-only-plugin");
    let (status, _) = request(
        &state,
        "administrator-key",
        "POST",
        "/api/v2/presets",
        Some(json!({"presets":[preset.clone(),invalid]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(state.store.custom_presets().await.unwrap().is_empty());
    assert_eq!(
        request(
            &state,
            "read-only-access-key",
            "POST",
            "/api/v2/presets",
            Some(json!({"presets":[preset.clone()]}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, result) = request(
        &state,
        "administrator-key",
        "POST",
        "/api/v2/presets",
        Some(json!({"presets":[preset.clone()]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["existing_services_changed"], false);
    assert_eq!(state.store.config_current().await.unwrap().0, 1);
    assert_eq!(
        state
            .store
            .get_backend("one")
            .await
            .unwrap()
            .unwrap()
            .priority,
        100
    );
    let (_, catalog) = request(
        &state,
        "read-only-access-key",
        "GET",
        "/api/v2/presets",
        None,
    )
    .await;
    assert!(
        catalog["presets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == "custom-gateway" && p["ui_group"] == "gateway")
    );
    let mut secret = preset;
    secret["config"]["options"] = json!({"headers":{"Authorization":"never-in-catalog"}});
    let (status, error) = request(
        &state,
        "administrator-key",
        "POST",
        "/api/v2/presets",
        Some(json!({"presets":[secret]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["error"]["code"], "preset_secret_forbidden");
    assert!(!error.to_string().contains("never-in-catalog"));
}
