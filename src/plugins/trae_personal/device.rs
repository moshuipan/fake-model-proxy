//! 按用户操作采集设备元数据；采集脚本与远端下载使用同一份源码。

use crate::{
    application::access::{Principal, Scope},
    backend::trae_personal::device::ClientDevice,
    core::AppError,
    state::AppState,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, header},
    routing::{get, post},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{process::Stdio, time::Duration};

const SCRIPT: &str = include_str!("../../../scripts/collect-trae-device.py");
const VERSION: &str = "fmp/client-device/v1";

/// 一个候选只包含同一客户端的设备上下文，标签不参与配置。
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Candidate {
    label: String,
    value: ClientDevice,
}

/// 下载脚本、采集按钮与手动导入共享的可移植报告。
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Report {
    api_version: String,
    plugin_id: String,
    candidates: Vec<Candidate>,
    #[serde(default)]
    warnings: Vec<String>,
}

/// 安装插件时挂载采集、导入校验与无凭证脚本下载入口。
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/plugins/trae_personal/device/capture", post(capture))
        .route("/api/plugins/trae_personal/device/validate", post(validate))
        .route("/api/plugins/trae_personal/device/collect.py", get(script))
        .layer(DefaultBodyLimit::max(256 * 1024))
}

/// 插件声明通用配置辅助组件，UI 无需识别厂商或复制设备校验。
pub(super) fn descriptor() -> Value {
    serde_json::json!({
        "id":"client_device", "title":"官方客户端设备信息", "option":"client_device",
        "capture_path":"/api/plugins/trae_personal/device/capture",
        "validate_path":"/api/plugins/trae_personal/device/validate",
        "script_path":"/api/plugins/trae_personal/device/collect.py",
        "script_filename":"collect-trae-device.py",
        "instructions":"自动获取读取 FMP 所在机器、当前运行用户的 TRAE CN 客户端。其他机器下载脚本，运行 python3 collect-trae-device.py --output trae-device.json（Windows 可用 py -3），再导入 JSON。需先启动官方客户端完成设备注册。只导出设备和版本元数据，不导出登录凭证，不联网。",
        "fields":[
            {"key":"device_id","title":"注册设备 ID"}, {"key":"device_brand","title":"设备型号"},
            {"key":"device_type","title":"系统类型"}, {"key":"os_version","title":"OS 版本"},
            {"key":"app_version","title":"应用版本"}, {"key":"ide_version","title":"IDE 内核版本"},
            {"key":"plugin_version","title":"构建版本"}
        ],
        "report_schema":schemars::schema_for!(Report),
        "value_schema":schemars::schema_for!(ClientDevice)
    })
}

/// 读取管理身份；采集要求操作权限，导入校验只要求读权限。
fn authorize(state: &AppState, headers: &HeaderMap, scope: Scope) -> Result<(), AppError> {
    Principal::authenticate(
        &state.config,
        headers
            .get("x-admin-key")
            .and_then(|value| value.to_str().ok())
            .or_else(|| {
                headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.strip_prefix("Bearer "))
            }),
    )?
    .require(scope)
}

/// 限制候选与诊断大小，所有有效设备都经过配置入口的同一语义校验。
fn normalize(input: Value) -> Result<Report, AppError> {
    let mut report = if input.get("api_version").is_some() {
        serde_json::from_value::<Report>(input)
            .map_err(|_| AppError::bad_request("设备报告格式无效，请导入采集脚本生成的 JSON"))?
    } else {
        let value = serde_json::from_value::<ClientDevice>(input)
            .map_err(|_| AppError::bad_request("设备 JSON 字段无效，请导入设备对象或采集报告"))?;
        Report {
            api_version: VERSION.into(),
            plugin_id: "trae_personal".into(),
            candidates: vec![Candidate {
                label: "手动导入的设备".into(),
                value,
            }],
            warnings: vec![],
        }
    };
    if report.api_version != VERSION
        || report.plugin_id != "trae_personal"
        || report.candidates.len() > 32
        || report.warnings.len() > 64
        || report.warnings.iter().any(|warning| warning.len() > 4096)
        || report
            .candidates
            .iter()
            .any(|candidate| candidate.label.len() > 4096)
    {
        return Err(AppError::bad_request("设备报告版本、插件或大小无效"));
    }
    for candidate in &report.candidates {
        candidate.value.validate()?;
    }
    if report.candidates.is_empty() && report.warnings.is_empty() {
        report
            .warnings
            .push("没有完整设备候选，请先启动官方客户端完成注册".into());
    }
    Ok(report)
}

/// 固定脚本通过独立 Python 进程执行；超时杀死子进程，不接受用户代码或路径。
async fn collect() -> Result<Report, AppError> {
    let mut process = tokio::process::Command::new(if cfg!(target_os = "windows") {
        "py"
    } else {
        "python3"
    });
    if cfg!(target_os = "windows") {
        process.arg("-3");
    }
    process
        .args(["-I", "-c", SCRIPT])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(25), process.output())
        .await
        .map_err(|_| AppError::bad_request("设备采集超时，请下载脚本在客户端机器执行"))?
        .map_err(|_| {
            AppError::bad_request("FMP 主机需要 Python 3.9+；也可下载脚本在其他机器采集后导入")
        })?;
    if !output.status.success() || output.stdout.len() > 256 * 1024 {
        return Err(AppError::bad_request(
            "设备采集未完成，请下载脚本并检查 Python 版本和客户端路径",
        ));
    }
    normalize(
        serde_json::from_slice(&output.stdout)
            .map_err(|_| AppError::bad_request("采集器未返回有效的设备报告"))?,
    )
}

/// 用户主动点击才读取主机客户端，不持久化或变更服务和账号。
async fn capture(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Report>, AppError> {
    authorize(&state, &headers, Scope::Operation)?;
    Ok(Json(collect().await?))
}

/// 服务端统一验证文件和粘贴 JSON；响应只填表单，保存继续走 ConfigService。
async fn validate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Value>,
) -> Result<Json<Report>, AppError> {
    authorize(&state, &headers, Scope::Read)?;
    Ok(Json(normalize(input)?))
}

/// 公开下载不含任何主机信息的固定脚本，浏览器将它保存为 Python 文件。
async fn script() -> ([(header::HeaderName, &'static str); 3], &'static str) {
    (
        [
            (header::CONTENT_TYPE, "text/x-python; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"collect-trae-device.py\"",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        SCRIPT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use serde_json::json;
    use std::{collections::BTreeSet, sync::Arc};
    use tower::ServiceExt;

    /// 构造不含真实凭证或设备的有效导入对象。
    fn device() -> Value {
        json!({"device_id":"99999999999999999999","device_brand":"Mac15,7",
            "device_type":"mac","os_version":"macOS 14.7.8","app_version":"3.4.24"})
    }

    /// 校验沿用配置语义，并拒绝报告中的其他插件及夹带秘密字段。
    #[test]
    fn import_uses_device_validation_and_rejects_unrelated_data() {
        let report = normalize(device()).unwrap();
        assert_eq!(report.candidates[0].value.device_id, "99999999999999999999");
        assert_eq!(report.candidates[0].value.plugin_version, "local");
        let mut bad = device();
        bad["device_id"] = json!("aha-local");
        assert!(normalize(bad).is_err());
        let mut bad = device();
        bad["os_version"] = json!("macOS\r\nx-token: secret");
        assert!(normalize(bad).is_err());
        let mut bad = device();
        bad["Token"] = json!("secret");
        assert!(!normalize(bad).err().unwrap().message().contains("secret"));
        assert!(
            normalize(json!({"api_version":VERSION,"plugin_id":"other",
            "candidates":[],"warnings":[]}))
            .is_err()
        );
    }

    /// 临时 Store 和只读身份保证采集权限测试不会接触用户配置。
    async fn state() -> AppState {
        let store = crate::store::Store::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let config = crate::config::Config {
            access_keys: vec![crate::application::access::AccessKey {
                id: "reader".into(),
                key: "read-only-device-key".into(),
                scopes: BTreeSet::from([Scope::Read]),
            }],
            ..Default::default()
        };
        AppState::new(
            Arc::new(config),
            store.clone(),
            Arc::new(crate::backend::Registry::new()),
            Arc::new(crate::records::RecordLog::default()),
            Arc::new(std::sync::RwLock::new(crate::tasks::TaskRegistry::new())),
            Arc::new(crate::routing::Router::new()),
            Arc::new(crate::usage::UsageLedger::new(store)),
        )
    }

    /// 走实际插件路由检查身份、权限、白名单下载及零持久化副作用。
    #[tokio::test]
    async fn routes_enforce_capture_permission_and_import_is_read_only() {
        let state = state().await;
        let router = crate::build_router(state.clone());
        for (path, key, expected) in [
            ("capture", None, StatusCode::UNAUTHORIZED),
            (
                "capture",
                Some("read-only-device-key"),
                StatusCode::FORBIDDEN,
            ),
            ("validate", Some("read-only-device-key"), StatusCode::OK),
        ] {
            let mut request = Request::builder()
                .method("POST")
                .uri(format!("/api/plugins/trae_personal/device/{path}"))
                .header("content-type", "application/json");
            if let Some(key) = key {
                request = request.header("x-admin-key", key);
            }
            let response = router
                .clone()
                .oneshot(request.body(Body::from(device().to_string())).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            if path == "validate" {
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let report: Report = serde_json::from_slice(&body).unwrap();
                assert_eq!(report.candidates[0].value.device_id, "99999999999999999999");
            }
        }
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/plugins/trae_personal/device/collect.py")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()[header::CONTENT_DISPOSITION]
                .to_str()
                .unwrap()
                .contains("collect-trae-device.py")
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), SCRIPT.as_bytes());
        assert!(state.store.list_backends().await.unwrap().is_empty());
        assert!(state.store.list_accounts().await.unwrap().is_empty());
    }
}
