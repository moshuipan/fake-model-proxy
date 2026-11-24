//! 编译期插件的可执行能力、隔离宿主和原子发布契约。

use fakemodelproxy::{
    accounts::Account,
    backend::Registry,
    core::{Capabilities, ModelInfo, TokenLimits},
    plugins::{ScopedHostServices, builtin},
    presets,
    store::{BackendRecord, Store},
};
use std::{path::Path, time::Duration};

/// 使用公开预设和假令牌构造服务，测试不需要网络访问。
fn record(plugin: &str) -> BackendRecord {
    let preset = presets::PRESETS
        .iter()
        .find(|preset| preset.plugin_id == plugin)
        .unwrap();
    BackendRecord {
        id: format!("test-{plugin}"),
        kind: plugin.into(),
        name: preset.name.clone(),
        base_url: preset.config.base_url.clone(),
        api_key: "fixture-only".into(),
        models: "model".into(),
        ..Default::default()
    }
}

/// 能力目录的每项声明必须对应实际 trait 视图；prepare 不接触上游。
#[tokio::test]
async fn every_registered_plugin_prepares_its_declared_capabilities() {
    let store = Store::open(Path::new(":memory:")).await.unwrap();
    let descriptors = builtin::registry().descriptors();
    assert_eq!(descriptors.len(), 9);
    for descriptor in descriptors {
        let config = record(&descriptor.id);
        let host = ScopedHostServices::legacy(&config, &[], store.clone(), Duration::from_secs(1));
        let runtime = builtin::registry().prepare(&config, host).unwrap();
        assert_eq!(runtime.inference.id(), config.id);
        assert_eq!(runtime.quota.is_some(), descriptor.features.quota);
        assert_eq!(
            runtime.credential_refresh.is_some(),
            descriptor.features.credential_refresh
        );
        assert_eq!(!runtime.tasks.is_empty(), descriptor.features.checkin);
        for task in runtime.tasks {
            assert_eq!(task.backend_id(), config.id);
        }
        assert!(
            descriptor.config_schema["properties"]["options"]["properties"]["routing"].is_object()
        );
    }
}

/// 新配置装配失败时旧 generation 保持可解析且不发布半套能力。
#[tokio::test]
async fn failed_preparation_keeps_previous_generation() {
    let store = Store::open(Path::new(":memory:")).await.unwrap();
    let registry = Registry::new();
    let valid = record("openai_compat");
    registry
        .reload(std::slice::from_ref(&valid), &[], &store)
        .unwrap();
    let previous = registry.get(&valid.id).unwrap();
    let invalid = BackendRecord {
        id: "broken".into(),
        kind: "missing-plugin".into(),
        ..valid.clone()
    };
    assert!(
        registry
            .reload(&[valid.clone(), invalid], &[], &store)
            .is_err()
    );
    assert!(std::sync::Arc::ptr_eq(
        &previous,
        &registry.get(&valid.id).unwrap()
    ));
    assert!(registry.get("broken").is_none());
    assert_eq!(registry.runtimes().len(), 1);
}

/// 作用域宿主不能读取或修改其他服务凭证，命名空间状态互不覆盖。
#[tokio::test]
async fn host_scopes_credentials_and_plugin_state() {
    let store = Store::open(Path::new(":memory:")).await.unwrap();
    let left = record("openai_compat");
    let right = BackendRecord {
        id: "other".into(),
        ..left.clone()
    };
    for config in [&left, &right] {
        store.upsert_backend(config).await.unwrap();
    }
    let account = Account {
        id: "a".into(),
        backend_id: right.id.clone(),
        name: "fixture".into(),
        credential: "secret".into(),
        enabled: true,
    };
    store.upsert_account(&account).await.unwrap();
    let a = ScopedHostServices::legacy(
        &left,
        std::slice::from_ref(&account),
        store.clone(),
        Duration::from_secs(1),
    );
    let b = ScopedHostServices::legacy(
        &right,
        std::slice::from_ref(&account),
        store.clone(),
        Duration::from_secs(1),
    );
    assert!(a.accounts().is_empty());
    assert!(a.get_account("a").await.unwrap().is_none());
    assert!(
        a.replace_credential("a", "secret", "overwritten")
            .await
            .is_err()
    );
    assert!(a.get_backend(&right.id).await.is_err());
    a.write_state("cursor", 1, &serde_json::json!({"page":2}))
        .await
        .unwrap();
    assert_eq!(a.read_state("cursor").await.unwrap().unwrap().0, 1);
    assert!(b.read_state("cursor").await.unwrap().is_none());
    assert_eq!(
        store.get_account("a").await.unwrap().unwrap().credential,
        "secret"
    );
}

/// 厂商约束归一化后核心不需要用 kind 猜测目录语义。
#[tokio::test]
async fn provider_limits_and_host_policy_remain_effective() {
    let store = Store::open(Path::new(":memory:")).await.unwrap();
    let registry = Registry::new();
    let qoder = BackendRecord {
        priority: 7,
        cost_tier: "paid".into(),
        ..record("qoder")
    };
    let workbuddy = record("workbuddy");
    registry
        .reload(&[qoder.clone(), workbuddy.clone()], &[], &store)
        .unwrap();
    let legacy = ModelInfo {
        id: "model".into(),
        display_name: None,
        price_factor: None,
        original_price_factor: None,
        price_valid_until: None,
        capabilities: Capabilities {
            context_window: Some(200_000),
            ..Default::default()
        },
        token_limits: TokenLimits::default(),
    };
    let q = registry.get(&qoder.id).unwrap();
    assert_eq!(q.model_limits(&legacy).max_input_tokens, Some(180_000));
    assert_eq!(q.priority(), 7);
    assert_eq!(q.cost_tier(), fakemodelproxy::backend::CostTier::Paid);
    let w = registry.get(&workbuddy.id).unwrap();
    assert_eq!(w.model_limits(&legacy).max_input_tokens, Some(200_000));
    assert_eq!(w.model_limits(&legacy).context_window, None);
}
