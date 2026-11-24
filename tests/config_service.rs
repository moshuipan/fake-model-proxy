//! 配置提交边界：真实 SQLite 与无网络兼容后端，验证失败原子性及凭证状态分离。

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use fakemodelproxy::{
    application::{config_service::ConfigService, config_types::*},
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

/// 隔离 SQLite 和运行状态，不接触真实账号或上游。
async fn state() -> AppState {
    let store = Store::open(Path::new(":memory:")).await.unwrap();
    let usage = Arc::new(UsageLedger::new(store.clone()));
    let config = Arc::new(Config::default());
    AppState::new(
        config,
        store,
        Arc::new(Registry::new()),
        Arc::new(RecordLog::new(100)),
        Arc::new(RwLock::new(TaskRegistry::new())),
        Arc::new(Router::new()),
        usage,
    )
}

/// 最小合法声明不包含秘密或远程探测。
fn service(id: &str) -> ServiceSpec {
    serde_json::from_value(json!({
        "api_version":"fmp/v2","kind":"Service","id":id,"name":id,
        "plugin_id":"openai_compat","plugin_config_version":1,"enabled":true,
        "config":{"base_url":"https://example.invalid/v1","models":["model"]},
        "policy":{"priority":20,"cost_tier":"unknown"}
    }))
    .unwrap()
}

/// 将一个服务放进声明式变更。
fn change(service: ServiceSpec) -> ConfigChange {
    ConfigChange {
        services: vec![service],
        ..Default::default()
    }
}

/// 合法计划一次提交；同键重试保持版本，同键不同计划被拒绝。
#[tokio::test]
async fn plan_apply_is_atomic_idempotent_and_revision_checked() {
    let state = state().await;
    let config = ConfigService::new(&state);
    let plan = config.plan(0, change(service("one"))).await.unwrap();
    assert!(state.store.list_backends().await.unwrap().is_empty());
    let result = config.apply(&plan.id, "first", "test").await.unwrap();
    assert_eq!(result.active_revision, 1);
    assert!(state.registry.get("one").is_some());
    assert_eq!(
        config
            .apply(&plan.id, "first", "test")
            .await
            .unwrap()
            .desired_revision,
        1
    );
    assert_eq!(
        config
            .plan(0, change(service("two")))
            .await
            .unwrap_err()
            .code(),
        "revision_conflict"
    );
    let plan2 = config.plan(1, change(service("two"))).await.unwrap();
    assert_eq!(
        config
            .apply(&plan2.id, "first", "test")
            .await
            .unwrap_err()
            .code(),
        "idempotency_conflict"
    );
    assert_eq!(state.store.config_current().await.unwrap().0, 1);
}

/// 未知字段、插件版本和缺失引用均产生字段诊断，失败不留下部分配置。
#[tokio::test]
async fn invalid_schema_and_missing_secret_never_commit() {
    let state = state().await;
    let config = ConfigService::new(&state);
    let mut input = service("invalid");
    input.config.options = json!({"not_a_real_option":true});
    let report = config.validate(change(input)).await.unwrap();
    assert!(!report.valid);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.path.contains("/config/options"))
    );
    let mut input = service("missing");
    input.credentials.api_key = Some(SecretRef {
        secret_ref: "store:does-not-exist".into(),
    });
    assert_eq!(
        config.plan(0, change(input)).await.unwrap_err().code(),
        "secret_missing"
    );
    let mut input = service("version");
    input.plugin_config_version = 900;
    assert_eq!(
        config.plan(0, change(input)).await.unwrap_err().code(),
        "plugin_incompatible"
    );
    assert_eq!(state.store.config_current().await.unwrap().0, 0);
    assert!(state.registry.list().is_empty());
}

/// 旧 CN 配置不会阻断网关或其他服务保存，提交保留区域和个人账号凭证。
#[tokio::test]
async fn legacy_trae_cn_options_allow_gateway_and_backend_saves() {
    use fakemodelproxy::backend::trae_personal::credential::{CLIENT_ID, Credential};

    let state = state().await;
    let trae = BackendRecord {
        id: "personal".into(),
        name: "TRAE CN".into(),
        kind: "trae_personal".into(),
        base_url: fakemodelproxy::backend::trae_personal::GATEWAY.into(),
        options: r#"{"region":"cn","routing":{"default_output_tokens":2048}}"#.into(),
        ..Default::default()
    };
    let mut other = BackendRecord {
        id: "other".into(),
        name: "Other".into(),
        base_url: "https://example.invalid/v1".into(),
        models: "model".into(),
        ..Default::default()
    };
    let credential = Credential {
        access_token: format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD
                .encode(json!({"data":{"id":"legacy-user"},"exp":9999999999u64}).to_string())
        ),
        refresh_token: "legacy-refresh".into(),
        subject: "legacy-user".into(),
        client_id: CLIENT_ID.into(),
        expires_at: 9999999999,
    }
    .serialize()
    .unwrap();
    let account = fakemodelproxy::accounts::Account {
        id: "personal-account".into(),
        backend_id: trae.id.clone(),
        name: "个人账号".into(),
        credential: credential.clone(),
        enabled: true,
    };
    state.store.upsert_backend(&trae).await.unwrap();
    state.store.upsert_backend(&other).await.unwrap();
    state.store.upsert_account(&account).await.unwrap();

    let config = ConfigService::new(&state);
    let baseline = config.current().await.unwrap();
    let report = config.validate(ConfigChange::default()).await.unwrap();
    assert!(report.valid, "{:?}", report.diagnostics);
    let mut gateway = baseline.document.gateway;
    gateway.max_attempts += 1;
    let plan = config
        .plan(
            0,
            ConfigChange {
                gateway: Some(gateway.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        config
            .apply(&plan.id, "gateway", "test")
            .await
            .unwrap()
            .active_revision,
        1
    );
    other.name = "Updated Other".into();
    assert_eq!(
        config.save_backend(&other).await.unwrap().active_revision,
        2
    );
    assert_eq!(
        config
            .current()
            .await
            .unwrap()
            .document
            .gateway
            .max_attempts,
        gateway.max_attempts
    );
    let saved = state.store.get_backend(&trae.id).await.unwrap().unwrap();
    let options: serde_json::Value = serde_json::from_str(&saved.options).unwrap();
    assert_eq!(options["region"], "cn");
    assert_eq!(options["routing"]["default_output_tokens"], 2048);
    assert_eq!(
        state
            .store
            .get_account(&account.id)
            .await
            .unwrap()
            .unwrap()
            .credential,
        credential
    );
    assert_eq!(
        state.registry.get(&other.id).unwrap().name(),
        "Updated Other"
    );
}

/// 导出 schema 和运行时配置只接受缺省、空字符串或 cn，拒绝区域类型错误。
#[tokio::test]
async fn trae_region_schema_matches_runtime_validation() {
    let state = state().await;
    let config = ConfigService::new(&state);
    for (options, valid) in [
        (json!({}), true),
        (json!({"region":""}), true),
        (json!({"region":"cn"}), true),
        (json!({"region":"global"}), false),
        (json!({"region":null}), false),
        (json!({"region":42}), false),
        (json!({"region":false}), false),
        (json!({"region":{}}), false),
        (json!({"region":[]}), false),
        (json!({"unknown":"private-option-value"}), false),
    ] {
        let mut input = service("personal");
        input.plugin_id = "trae_personal".into();
        input.config.base_url = fakemodelproxy::backend::trae_personal::GATEWAY.into();
        input.config.options = options.clone();
        let report = config.validate(change(input)).await.unwrap();
        assert_eq!(report.valid, valid, "{options}: {:?}", report.diagnostics);
        let record = BackendRecord {
            options: options.to_string(),
            ..Default::default()
        };
        assert_eq!(
            fakemodelproxy::backend::trae_personal::profile(&record).is_ok(),
            valid,
            "{options}"
        );
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("private-option-value")
        );
    }
}

/// 设备 schema 与运行时共用完整字段和 HTTP 安全校验，错误不回显输入值。
#[tokio::test]
async fn trae_device_schema_matches_runtime_and_rebuilds_connection() {
    let state = state().await;
    let config = ConfigService::new(&state);
    let device = json!({
        "device_id":"1234567890123456","device_brand":"TestMac1,2",
        "device_type":"mac","os_version":"macOS 14.0","app_version":"3.4.20"
    });
    let mut variants = vec![
        (device.clone(), true),
        (Value::Null, true),
        (json!({}), false),
    ];
    for (field, value) in [
        ("device_id", json!("0")),
        ("device_id", json!("aha-unregistered")),
        ("device_id", json!("123456789012345678901")),
        ("device_brand", json!("private-option-value\r\ninjected")),
        ("device_brand", json!(" ")),
        ("device_type", json!("darwin")),
        ("os_version", json!("macOS\t14.0")),
        ("os_version", json!(42)),
        ("app_version", json!("")),
        ("app_version", json!("1".repeat(65))),
        ("machine_id", json!("bad/id")),
        ("app_type", json!("unknown")),
        ("plugin_version", json!("invalid\nversion")),
        ("unknown", json!("private-option-value")),
    ] {
        let mut changed = device.clone();
        changed[field] = value;
        variants.push((changed, false));
    }
    for (device, valid) in variants {
        let options = json!({"region":"cn","client_device":device});
        let mut input = service("personal");
        input.plugin_id = "trae_personal".into();
        input.config.base_url = fakemodelproxy::backend::trae_personal::GATEWAY.into();
        input.config.options = options.clone();
        let report = config.validate(change(input)).await.unwrap();
        assert_eq!(report.valid, valid, "{:?}", report.diagnostics);
        assert_eq!(
            fakemodelproxy::backend::trae_personal::profile(&BackendRecord {
                options: options.to_string(),
                ..Default::default()
            })
            .is_ok(),
            valid
        );
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("private-option-value")
        );
    }
    let mut input = service("personal");
    input.plugin_id = "trae_personal".into();
    input.config.base_url = fakemodelproxy::backend::trae_personal::GATEWAY.into();
    input.config.options = json!({"client_device":device});
    let plan = config.plan(0, change(input.clone())).await.unwrap();
    config.apply(&plan.id, "device-add", "test").await.unwrap();
    input.config.options["client_device"]["app_version"] = json!("3.4.21");
    let plan = config.plan(1, change(input)).await.unwrap();
    assert!(plan.changes.iter().any(|change| change.rebuild_connection));
    config
        .apply(&plan.id, "device-update", "test")
        .await
        .unwrap();
    let record = state.store.get_backend("personal").await.unwrap().unwrap();
    let stored: Value = serde_json::from_str(&record.options).unwrap();
    assert_eq!(stored["client_device"]["app_version"], "3.4.21");
}

/// 未知字段诊断提供逐字段 JSON Pointer，转义路径字符并且不包含输入值。
#[tokio::test]
async fn unexpected_option_diagnostics_identify_keys_without_values() {
    let state = state().await;
    let config = ConfigService::new(&state);
    let mut input = service("invalid");
    input.config.options = json!({
        "unknown":"private-option-value",
        "path/with~escape":{"nested":"private-nested-value"}
    });
    let report = config.validate(change(input.clone())).await.unwrap();
    assert!(!report.valid);
    for path in [
        "/services/0/config/options/unknown",
        "/services/0/config/options/path~1with~0escape",
    ] {
        assert!(
            report.diagnostics.iter().any(|d| d.code == "schema_invalid"
                && d.path == path
                && d.message
                    .contains("/properties/options/additionalProperties")),
            "{:?}",
            report.diagnostics
        );
    }
    let error = config
        .save_backend(&BackendRecord {
            id: input.id,
            name: input.name,
            base_url: input.config.base_url,
            options: input.config.options.to_string(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), "schema_invalid");
    assert!(error.message().contains("/config/options/"));
    let text = format!(
        "{}{}",
        serde_json::to_string(&report).unwrap(),
        error.message()
    );
    assert!(!text.contains("private-option-value") && !text.contains("private-nested-value"));
    assert_eq!(state.store.config_current().await.unwrap().0, 0);
    assert!(state.store.list_backends().await.unwrap().is_empty());
}

/// 凭证轮换不使计划失效，回滚以新 revision 恢复配置且保留最新 token。
#[tokio::test]
async fn rotation_is_runtime_state_and_rollback_does_not_restore_old_token() {
    let state = state().await;
    state
        .store
        .upsert_backend(&BackendRecord {
            id: "one".into(),
            name: "One".into(),
            base_url: "https://example.invalid/v1".into(),
            models: "model".into(),
            api_key: "api-secret".into(),
            options: r#"{"headers":{"Authorization":"header-secret"}}"#.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    state
        .store
        .upsert_account(&fakemodelproxy::accounts::Account {
            id: "account".into(),
            backend_id: "one".into(),
            name: "Account".into(),
            credential: "old-token".into(),
            enabled: true,
        })
        .await
        .unwrap();
    let config = ConfigService::new(&state);
    let baseline = config.current().await.unwrap();
    let text = serde_json::to_string(&baseline).unwrap();
    assert!(
        !text.contains("api-secret")
            && !text.contains("header-secret")
            && !text.contains("old-token")
    );
    let mut service = baseline.document.services[0].clone();
    service.name = "Updated".into();
    let plan = config.plan(0, change(service)).await.unwrap();
    assert!(
        state
            .store
            .replace_credential("account", "old-token", "new-token")
            .await
            .unwrap()
    );
    config.apply(&plan.id, "update", "test").await.unwrap();
    assert_eq!(
        state
            .store
            .get_account("account")
            .await
            .unwrap()
            .unwrap()
            .credential,
        "new-token"
    );
    let rollback = config.rollback_plan(1, 0).await.unwrap();
    config
        .apply(&rollback.id, "rollback", "test")
        .await
        .unwrap();
    assert_eq!(
        state.store.get_backend("one").await.unwrap().unwrap().name,
        "One"
    );
    assert_eq!(
        state
            .store
            .get_account("account")
            .await
            .unwrap()
            .unwrap()
            .credential,
        "new-token"
    );
    assert_eq!(config.current().await.unwrap().active_revision, 2);
}

/// 两个同基线计划只有一个能提交，落后请求不会覆盖获胜者。
#[tokio::test]
async fn competing_plans_have_one_winner() {
    let state = state().await;
    let config = ConfigService::new(&state);
    let first = config.plan(0, change(service("first"))).await.unwrap();
    let second = config.plan(0, change(service("second"))).await.unwrap();
    config.apply(&second.id, "second", "test").await.unwrap();
    assert_eq!(
        config
            .apply(&first.id, "first", "test")
            .await
            .unwrap_err()
            .code(),
        "revision_conflict"
    );
    assert!(state.registry.get("second").is_some());
    assert!(state.registry.get("first").is_none());
}

/// 一次导入包含非法插件时，前面合法服务和网关设置也不能部分提交。
#[tokio::test]
async fn legacy_import_failure_is_atomic_and_headers_remain_redacted() {
    let state = state().await;
    let config = ConfigService::new(&state);
    let record = BackendRecord {
        id: "one".into(),
        name: "One".into(),
        base_url: "https://example.invalid/v1".into(),
        api_key: "private-api".into(),
        options: r#"{"headers":{"Authorization":"private-header"}}"#.into(),
        ..Default::default()
    };
    config.save_backend(&record).await.unwrap();
    let mut bad = record.clone();
    bad.id = "bad".into();
    bad.kind = "not_registered".into();
    let bundle =
        fakemodelproxy::config_bundle::ConfigBundle::build(vec![record.clone(), bad], vec![], true);
    assert!(config.import_bundle(&bundle).await.is_err());
    assert_eq!(state.store.config_current().await.unwrap().0, 1);
    assert_eq!(state.store.list_backends().await.unwrap().len(), 1);
    let safe = fakemodelproxy::config_bundle::ConfigBundle::build(vec![record], vec![], false);
    let text = serde_json::to_string(&safe).unwrap();
    assert!(!text.contains("private-header") && !text.contains("private-api"));
    config.import_bundle(&safe).await.unwrap();
    let saved = state.store.get_backend("one").await.unwrap().unwrap();
    assert_eq!(saved.api_key, "private-api");
    assert!(saved.options.contains("private-header"));
}

/// 仅改名称与优先级复用底层可选能力，避免重建登录态和账号池。
#[tokio::test]
async fn metadata_edit_reuses_connection_and_preserves_old_generation() {
    let state = state().await;
    let config = ConfigService::new(&state);
    let record = BackendRecord {
        id: "qoder".into(),
        name: "Qoder".into(),
        kind: "qoder".into(),
        base_url: "https://qoder.com".into(),
        ..Default::default()
    };
    config.save_backend(&record).await.unwrap();
    let before = state.registry.runtime("qoder").unwrap();
    let old_generation = state.registry.fork();
    let mut edit = config.current().await.unwrap().document.services.remove(0);
    edit.name = "My Qoder".into();
    edit.policy.priority = 9;
    edit.policy.routing.qps = 3;
    let plan = config.plan(1, change(edit)).await.unwrap();
    assert!(!plan.changes[0].rebuild_connection);
    config.apply(&plan.id, "edit", "test").await.unwrap();
    let after = state.registry.runtime("qoder").unwrap();
    assert!(Arc::ptr_eq(
        before.quota.as_ref().unwrap(),
        after.quota.as_ref().unwrap()
    ));
    assert_eq!(old_generation.get("qoder").unwrap().name(), "Qoder");
    assert_eq!(state.registry.get("qoder").unwrap().name(), "My Qoder");
    assert_eq!(state.registry.get("qoder").unwrap().priority(), 9);
    assert_eq!(state.registry.get("qoder").unwrap().qps(), 3);
    assert_eq!(old_generation.get("qoder").unwrap().qps(), 0);
}

/// 九种插件混合配置支持逐服务保存和网关保存，宿主限频不进入专属 options。
#[tokio::test]
async fn all_builtin_services_and_gateway_save_with_qps() {
    let state = state().await;
    let config = ConfigService::new(&state);
    let presets: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../presets/builtin.json")).unwrap();
    let ids = [
        "openai_compat",
        "modelscope",
        "qoder",
        "workbuddy",
        "autoglm",
        "minimax_code",
        "monkeycode",
        "raccoon",
        "trae_personal",
    ];
    let mut inputs = Vec::new();
    for id in ids {
        let preset = presets.iter().find(|p| p["plugin_id"] == id).unwrap();
        let mut input = service(id);
        input.plugin_id = id.into();
        input.enabled = false;
        input.config = serde_json::from_value(preset["config"].clone()).unwrap();
        if id == "trae_personal" {
            input.config.options = json!({"region":"cn"});
        }
        input.policy.routing.qps = 7;
        input.policy.routing.default_output_tokens = 2048;
        inputs.push(input);
    }
    let plan = config
        .plan(
            0,
            ConfigChange {
                services: inputs.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    config.apply(&plan.id, "all", "test").await.unwrap();
    for (index, input) in inputs.iter_mut().enumerate() {
        input.name = format!("Edited {}", input.id);
        input.policy.routing.qps = 2 + index as u32;
        let record = state.store.get_backend(&input.id).await.unwrap().unwrap();
        let plan = config
            .plan(index as u64 + 1, change(input.clone()))
            .await
            .unwrap();
        assert!(plan.changes.iter().all(|change| !change.rebuild_connection));
        config
            .apply(&plan.id, &format!("edit-{index}"), "test")
            .await
            .unwrap();
        let saved = state.store.get_backend(&input.id).await.unwrap().unwrap();
        assert_eq!(record.base_url, saved.base_url);
        let options: serde_json::Value = serde_json::from_str(&saved.options).unwrap();
        assert_eq!(options["routing"]["qps"], input.policy.routing.qps);
    }
    let current = config.current().await.unwrap();
    let mut gateway = current.document.gateway;
    gateway.max_attempts += 1;
    let plan = config
        .plan(
            current.desired_revision,
            ConfigChange {
                gateway: Some(gateway),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    config.apply(&plan.id, "gateway-all", "test").await.unwrap();
    let saved = config.current().await.unwrap().document.services;
    for input in inputs {
        let output = saved.iter().find(|s| s.id == input.id).unwrap();
        assert_eq!(output.policy.routing.qps, input.policy.routing.qps);
        assert_eq!(output.config.options, input.config.options);
        assert_eq!(output.policy.routing.default_output_tokens, 2048);
    }
}

/// 旧 v1 限频选项经配置服务迁入 policy，v2 schema 使用同一整数范围。
#[tokio::test]
async fn legacy_qps_and_policy_schema_share_the_contract() {
    let state = state().await;
    let config = ConfigService::new(&state);
    let record = BackendRecord {
        id: "legacy".into(),
        name: "Legacy".into(),
        base_url: "https://example.invalid/v1".into(),
        models: "model".into(),
        options: r#"{"routing":{"qps":2}}"#.into(),
        ..Default::default()
    };
    config.save_backend(&record).await.unwrap();
    assert_eq!(state.registry.get("legacy").unwrap().qps(), 2);
    let current = config.current().await.unwrap();
    assert_eq!(current.document.services[0].policy.routing.qps, 2);
    assert_eq!(current.document.services[0].config.options, json!({}));
    let schema = schema::<ServiceSpec>();
    let validator = jsonschema::validator_for(&schema).unwrap();
    for (qps, valid) in [
        (json!(0), true),
        (json!(1), true),
        (json!(10_000), true),
        (json!(-1), false),
        (json!(1.5), false),
        (json!(10_001), false),
        (json!("2"), false),
    ] {
        let mut value = serde_json::to_value(service("test")).unwrap();
        value["policy"]["qps"] = qps;
        assert_eq!(validator.is_valid(&value), valid, "{value}");
    }
}

/// 已有非法服务会阻断所有保存，但诊断必须标出实际服务和插件，不能回显值。
#[tokio::test]
async fn invalid_existing_service_is_identified_in_all_save_paths() {
    let state = state().await;
    state
        .store
        .upsert_backend(&BackendRecord {
            id: "broken".into(),
            name: "Broken".into(),
            kind: "raccoon".into(),
            base_url: "https://xiaohuanxiong.com".into(),
            options: r#"{"unknown":"private-option-value"}"#.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let config = ConfigService::new(&state);
    let current = config.current().await.unwrap();
    for input in [
        change(service("other")),
        ConfigChange {
            gateway: Some(current.document.gateway),
            ..Default::default()
        },
    ] {
        let report = config.validate(input).await.unwrap();
        assert!(!report.valid);
        let diagnostic = report
            .diagnostics
            .iter()
            .find(|d| d.path.ends_with("/options/unknown"))
            .unwrap();
        assert!(diagnostic.message.contains("服务 broken（插件 raccoon）"));
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("private-option-value")
        );
    }
}
