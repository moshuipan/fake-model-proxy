//! 调用名称端到端回归：稳定性、兼容性、持久化和真实协议路由。

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use fakemodelproxy::backend::Registry;
use fakemodelproxy::config::Config;
use fakemodelproxy::core::ModelInfo;
use fakemodelproxy::model_names::ModelAlias;
use fakemodelproxy::state::AppState;
use fakemodelproxy::store::{BackendRecord, Store};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, RwLock};
use tower::ServiceExt;

/// 假上游仅接受原始模型键，记录全部请求用于核对三协议及流式分支。
async fn upstream(
    State(models): State<Arc<Mutex<Vec<String>>>>,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(body["model"], "raw_key");
    models
        .lock()
        .unwrap()
        .push(body["model"].as_str().unwrap().into());
    if body["stream"] == true {
        Response::builder().header("content-type", "text/event-stream").body(Body::from(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"OK\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
        )).unwrap()
    } else {
        Response::builder().header("content-type", "application/json").body(Body::from(json!({
            "choices":[{"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        }).to_string())).unwrap()
    }
}

/// 用临时 SQLite 和本地假上游验证，完全不访问真实服务。
struct Fixture {
    state: AppState,
    models: Arc<Mutex<Vec<String>>>,
}

impl Fixture {
    /// 两个不同服务提供相同原始键和显示名，主名称按渠道区分。
    async fn new() -> Self {
        let models = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/v1/chat/completions", post(upstream))
            .route(
                "/v1/models",
                get(|| async { Json(json!({"data":[{"id":"raw_key"}]})) }),
            )
            .with_state(models.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        for (id, name) in [
            ("internal-a", "My Gateway"),
            ("internal-b", "Other Gateway"),
        ] {
            store
                .upsert_backend(&BackendRecord {
                    id: id.into(),
                    name: name.into(),
                    models: "raw_key".into(),
                    base_url: base.clone(),
                    cost_tier: "free".into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            store
                .sync_model_catalog(
                    id,
                    &[ModelInfo {
                        id: "raw_key".into(),
                        display_name: Some("GLM-5.3".into()),
                        price_factor: None,
                        original_price_factor: None,
                        price_valid_until: None,
                        capabilities: Default::default(),
                        token_limits: Default::default(),
                    }],
                )
                .await
                .unwrap();
        }
        let state = AppState::new(
            Arc::new(Config::default()),
            store.clone(),
            Arc::new(Registry::new()),
            Arc::new(fakemodelproxy::records::RecordLog::default()),
            Arc::new(RwLock::new(fakemodelproxy::tasks::TaskRegistry::new())),
            Arc::new(fakemodelproxy::routing::Router::new()),
            Arc::new(fakemodelproxy::usage::UsageLedger::new(store)),
        );
        state.reload_backends().await.unwrap();
        Self { state, models }
    }

    /// 通过实际路由收集协议 JSON 或 SSE。
    async fn send(&self, method: &str, path: &str, body: Value) -> (StatusCode, String) {
        let response = fakemodelproxy::build_router(self.state.clone())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    /// 提交名称编辑，明确传递编辑时读取到的旧名称。
    async fn rename(&self, alias: &str, previous: &str) -> StatusCode {
        self.send("PUT", "/api/model-aliases", json!({
            "backend_id":"internal-a","model_id":"raw_key","alias":alias,"previous_alias":previous,
        })).await.0
    }
}

/// 六个协议分支使用公开名称，上游只收到原始键，记录仍保存原始定位。
#[tokio::test]
async fn aliases_route_all_protocols_and_preserve_audit() {
    let fixture = Fixture::new().await;
    let alias = "my-gateway-glm-5.3";
    for stream in [false, true] {
        for protocol in ["chat/completions", "messages", "responses"] {
            let body = if protocol == "responses" {
                json!({"model":alias,"input":"hi","stream":stream,"max_output_tokens":16})
            } else {
                json!({"model":alias,"messages":[{"role":"user","content":"hi"}],"stream":stream,"max_tokens":16})
            };
            let (status, response) = fixture.send("POST", &format!("/v1/{protocol}"), body).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            assert!(response.contains(alias), "{response}");
            assert!(!response.contains("internal-a/raw_key"), "{response}");
        }
    }
    assert_eq!(fixture.models.lock().unwrap().len(), 6);
    let (_, records) = fixture.send("GET", "/api/records", Value::Null).await;
    let records: Value = serde_json::from_str(&records).unwrap();
    for record in records["records"].as_array().unwrap() {
        assert_eq!(record["requested_model"], alias);
        assert_eq!(record["resolved_model"], "internal-a/raw_key");
        assert_eq!(record["attempts"][0]["model"], "internal-a/raw_key");
    }
}

/// 改服务名、显示名、顺序和重新装配都不能重新生成已使用的名称。
#[tokio::test]
async fn alias_is_stable_and_rename_preserves_old_clients() {
    let fixture = Fixture::new().await;
    let old = "my-gateway-glm-5.3";
    let mut record = fixture
        .state
        .store
        .get_backend("internal-a")
        .await
        .unwrap()
        .unwrap();
    record.name = "Renamed".into();
    fixture
        .state
        .config_service()
        .save_backend(&record)
        .await
        .unwrap();
    fixture.state.reload_backends().await.unwrap();
    assert_eq!(
        fixture.state.registry.public_model_id("internal-a/raw_key"),
        old
    );
    assert_eq!(fixture.rename("daily-glm", old).await, StatusCode::OK);
    assert_eq!(
        fixture.rename("stale-edit", old).await,
        StatusCode::BAD_REQUEST
    );
    for name in [old, "daily-glm", "internal-a/raw_key"] {
        let (backend, model) = fixture.state.registry.resolve(name).unwrap();
        assert_eq!(backend.id(), "internal-a");
        assert_eq!(model, "raw_key");
    }
    assert_eq!(
        fixture.state.registry.public_model_id("internal-a/raw_key"),
        "daily-glm"
    );
    let (status, _) = fixture
        .send(
            "POST",
            "/v1/chat/completions",
            json!({"model":old,"messages":[{"role":"user","content":"hi"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

/// 冲突、自动路由保留名称和不合法字符被拒绝，不修改当前映射。
#[tokio::test]
async fn rejects_conflicts_reserved_names_and_native_shadowing() {
    let fixture = Fixture::new().await;
    for alias in [
        "other-gateway-glm-5.3",
        "auto",
        "auto:cost",
        "Auto",
        "a/b",
        "raw_key",
    ] {
        assert_eq!(
            fixture.rename(alias, "my-gateway-glm-5.3").await,
            StatusCode::BAD_REQUEST,
            "{alias}"
        );
    }
    assert_eq!(
        fixture.state.registry.public_model_id("internal-a/raw_key"),
        "my-gateway-glm-5.3"
    );
}

/// 停用或移除白名单后，当前名称和历史名称都不能绕过服务状态。
#[tokio::test]
async fn disabled_removed_and_deleted_models_keep_reserved_names() {
    let fixture = Fixture::new().await;
    let old = "my-gateway-glm-5.3";
    assert_eq!(fixture.rename("daily-glm", old).await, StatusCode::OK);
    let mut record = fixture
        .state
        .store
        .get_backend("internal-a")
        .await
        .unwrap()
        .unwrap();
    record.enabled = false;
    fixture
        .state
        .config_service()
        .save_backend(&record)
        .await
        .unwrap();
    fixture.state.reload_backends().await.unwrap();
    assert!(fixture.state.registry.resolve(old).is_err());
    assert!(fixture.state.registry.resolve("daily-glm").is_err());
    let (_, models) = fixture.send("GET", "/v1/models", Value::Null).await;
    assert!(!models.contains("daily-glm"));
    record.enabled = true;
    record.models = "different".into();
    fixture
        .state
        .config_service()
        .save_backend(&record)
        .await
        .unwrap();
    fixture.state.reload_backends().await.unwrap();
    assert!(fixture.state.registry.resolve("daily-glm").is_err());
    fixture
        .state
        .config_service()
        .delete_backend("internal-a")
        .await
        .unwrap();
    fixture.state.reload_backends().await.unwrap();
    let (_, aliases) = fixture.send("GET", "/api/model-aliases", Value::Null).await;
    assert!(aliases.contains("daily-glm"));
    let conflict = ModelAlias {
        alias: "daily-glm".into(),
        backend_id: "internal-b".into(),
        model_id: "raw_key".into(),
        is_primary: true,
    };
    assert!(
        fixture
            .state
            .store
            .import_model_aliases(&[conflict], &[])
            .await
            .is_err()
    );
}

/// 配置迁移携带历史名称，旧格式继续兼容；一次冲突拒绝全部映射。
#[tokio::test]
async fn bundle_roundtrip_and_conflict_rollback() {
    let fixture = Fixture::new().await;
    assert_eq!(
        fixture.rename("daily-glm", "my-gateway-glm-5.3").await,
        StatusCode::OK
    );
    let (_, exported) = fixture.send("GET", "/api/config/export", Value::Null).await;
    let bundle: fakemodelproxy::config_bundle::ConfigBundle =
        serde_json::from_str(&exported).unwrap();
    bundle.validate().unwrap();
    let target = Store::open(std::path::Path::new(":memory:")).await.unwrap();
    target
        .import_model_aliases(&bundle.model_aliases, &bundle.backends)
        .await
        .unwrap();
    for backend in &bundle.backends {
        target.upsert_backend(backend).await.unwrap();
    }
    target
        .ensure_model_aliases(&target.list_backends().await.unwrap(), &Default::default())
        .await
        .unwrap();
    assert_eq!(
        target.list_model_aliases().await.unwrap(),
        bundle.model_aliases
    );
    let legacy: fakemodelproxy::config_bundle::ConfigBundle = serde_json::from_value(json!({
        "version":1,"backends":[],"accounts":[]
    }))
    .unwrap();
    legacy.validate().unwrap();
    let mut incoming = bundle.model_aliases[0].clone();
    incoming.alias = "new-alias".into();
    let mut conflict = bundle.model_aliases[0].clone();
    conflict.backend_id = "different".into();
    let before = target.list_model_aliases().await.unwrap();
    assert!(
        target
            .import_model_aliases(&[incoming, conflict], &[])
            .await
            .is_err()
    );
    assert_eq!(target.list_model_aliases().await.unwrap(), before);
}

/// 首次发现动态模型时可直接调用，重复发现和重载保持同一个名称。
#[tokio::test]
async fn dynamic_discovery_and_concurrent_default_collisions_are_stable() {
    let fixture = Fixture::new().await;
    let mut record = fixture
        .state
        .store
        .get_backend("internal-a")
        .await
        .unwrap()
        .unwrap();
    record.models.clear();
    fixture
        .state
        .config_service()
        .save_backend(&record)
        .await
        .unwrap();
    fixture.state.reload_backends().await.unwrap();
    let (one, two) = tokio::join!(fixture.state.public_models(), fixture.state.public_models());
    assert_eq!(
        one.unwrap().iter().map(|m| &m.id).collect::<Vec<_>>(),
        two.unwrap().iter().map(|m| &m.id).collect::<Vec<_>>()
    );
    assert!(fixture.state.registry.resolve("my-gateway-glm-5.3").is_ok());
    let record = BackendRecord {
        id: "third".into(),
        name: "My Gateway".into(),
        models: "raw_key".into(),
        ..record
    };
    let catalog = fixture.state.store.list_model_catalogs().await.unwrap();
    fixture.state.store.upsert_backend(&record).await.unwrap();
    fixture
        .state
        .store
        .cache_routing_models(&record, &catalog["internal-a"])
        .await
        .unwrap();
    fixture
        .state
        .config_service()
        .save_backend(&record)
        .await
        .unwrap();
    let catalog = fixture.state.store.list_model_catalogs().await.unwrap();
    fixture
        .state
        .store
        .sync_model_catalog("third", &catalog["internal-a"])
        .await
        .unwrap();
    fixture.state.reload_backends().await.unwrap();
    assert_eq!(
        fixture.state.registry.public_model_id("third/raw_key"),
        "my-gateway-glm-5.3-2"
    );
    let dynamic = BackendRecord {
        id: "dynamic".into(),
        name: "Dynamic".into(),
        models: String::new(),
        ..fixture
            .state
            .store
            .get_backend("internal-a")
            .await
            .unwrap()
            .unwrap()
    };
    fixture
        .state
        .config_service()
        .save_backend(&dynamic)
        .await
        .unwrap();
    fixture.state.reload_backends().await.unwrap();
    assert!(fixture.state.registry.resolve("dynamic-raw_key").is_err());
    let (one, two) = tokio::join!(fixture.state.public_models(), fixture.state.public_models());
    assert!(
        one.unwrap()
            .iter()
            .any(|model| model.id == "dynamic-raw_key")
    );
    assert!(
        two.unwrap()
            .iter()
            .any(|model| model.id == "dynamic-raw_key")
    );
    assert_eq!(
        fixture
            .state
            .registry
            .resolve("dynamic-raw_key")
            .unwrap()
            .0
            .id(),
        "dynamic"
    );
}

/// 数据库重开后主名称及历史名称原样恢复，不依赖目录缓存存在。
#[tokio::test]
async fn names_survive_database_reopen() {
    let dir = std::env::temp_dir().join(format!("fmp-aliases-{}", uuid::Uuid::new_v4()));
    let path = dir.join("test.sqlite");
    let store = Store::open(&path).await.unwrap();
    let record = BackendRecord {
        id: "b".into(),
        name: "auto".into(),
        models: "glm".into(),
        ..Default::default()
    };
    store.upsert_backend(&record).await.unwrap();
    store
        .ensure_model_aliases(&[record], &Default::default())
        .await
        .unwrap();
    let alias = store.list_model_aliases().await.unwrap()[0].alias.clone();
    fakemodelproxy::model_names::validate_alias(&alias).unwrap();
    store
        .rename_model_alias("b", "glm", "daily-glm", &alias)
        .await
        .unwrap();
    let expected = store.list_model_aliases().await.unwrap();
    drop(store);
    let reopened = Store::open(&path).await.unwrap();
    assert_eq!(reopened.list_model_aliases().await.unwrap(), expected);
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}
