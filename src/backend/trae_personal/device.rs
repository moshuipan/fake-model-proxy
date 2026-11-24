//! 显式配置的官方客户端设备上下文；设备注册由官方客户端完成。

use crate::core::AppError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 一份服务对应一台客户端；同服务账号共享物理设备，凭证和每日记录仍按账号隔离。
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClientDevice {
    /// 官方客户端注册完成后的 device ID，不是 machine ID 或本地备用 ID。
    #[schemars(length(min = 1, max = 20), regex(pattern = "^[1-9][0-9]{0,19}$"))]
    pub device_id: String,
    /// 官方 commonParams.device_model，例如 Mac 的硬件型号。
    #[schemars(length(min = 1, max = 128))]
    pub device_brand: String,
    /// 官方 commonParams.os_name。
    #[schemars(regex(pattern = "^(mac|windows|linux)$"))]
    pub device_type: String,
    /// 官方 commonParams.os_version，按客户端原字符串发送。
    #[schemars(length(min = 1, max = 256))]
    pub os_version: String,
    /// 官方 product.appVersion；不是 VS Code 版本或 FMP 版本。
    #[schemars(length(min = 1, max = 64))]
    pub app_version: String,
    /// 官方 extension API 的 version（product.version），与 GUI appVersion 分开。
    #[serde(default)]
    #[schemars(length(max = 64))]
    pub ide_version: String,
    /// 官方授权使用的 telemetry.machineId；缺省仅使用 FMP 服务的稳定机器标识。
    #[serde(default)]
    #[schemars(length(max = 128), regex(pattern = "^[A-Za-z0-9_-]*$"))]
    pub machine_id: String,
    /// 官方 product.tronBuildVersion，缺省沿用官方的 local 回退值。
    #[serde(default = "local_version")]
    #[schemars(length(min = 1, max = 64))]
    pub plugin_version: String,
    /// 官方授权渠道；设备配置不改变个人 CN 授权域。
    #[serde(default = "stable")]
    #[schemars(regex(pattern = "^(stable|insider)$"))]
    pub app_type: String,
}

/// 没有官方构建号时不编造版本。
fn local_version() -> String {
    "local".into()
}

/// 个人桌面客户端默认使用稳定渠道。
fn stable() -> String {
    "stable".into()
}

impl ClientDevice {
    /// 所有入口共用验证；不把非法输入或凭证写入诊断。
    pub(crate) fn validate(&self) -> Result<(), AppError> {
        if self.device_id.is_empty()
            || self.device_id.len() > 20
            || self.device_id.starts_with('0')
            || !self.device_id.bytes().all(|byte| byte.is_ascii_digit())
            || !matches!(self.device_type.as_str(), "mac" | "windows" | "linux")
            || !matches!(self.app_type.as_str(), "stable" | "insider")
            || self.machine_id.len() > 128
            || !self
                .machine_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(AppError::bad_request("TRAE 官方客户端设备标识或类型无效"));
        }
        for (name, value, limit) in
            [
                ("device_brand", &self.device_brand, 128),
                ("os_version", &self.os_version, 256),
                ("app_version", &self.app_version, 64),
                ("plugin_version", &self.plugin_version, 64),
            ]
            .into_iter()
            .chain((!self.ide_version.is_empty()).then_some(("ide_version", &self.ide_version, 64)))
        {
            if value.is_empty()
                || value.len() > limit
                || value.trim() != value
                || value.chars().any(char::is_control)
                || reqwest::header::HeaderValue::from_str(value).is_err()
            {
                return Err(AppError::bad_request(format!(
                    "TRAE client_device.{name} 须为有效且无控制字符的客户端参数"
                )));
            }
        }
        Ok(())
    }

    /// 余额与签到按官方商业活动 API 编码相同五个设备头。
    pub(crate) fn headers(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .header("x-device-id", &self.device_id)
            .header("x-device-brand", &self.device_brand)
            .header("x-device-type", &self.device_type)
            .header("x-os-version", &self.os_version)
            .header("x-app-version", &self.app_version)
    }

    /// 原生 AI 扩展使用 encodeURIComponent 编码设备参数，商业活动 API 仍发送原值。
    pub(crate) fn native_headers(
        &self,
        request: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        let mut request = request
            .header("x-device-id", component(&self.device_id))
            .header("x-device-brand", component(&self.device_brand))
            .header("x-device-type", component(&self.device_type))
            .header("x-os-version", component(&self.os_version))
            .header("x-ide-version-type", &self.app_type);
        if !self.machine_id.is_empty() {
            request = request.header("x-machine-id", component(&self.machine_id));
        }
        if !self.ide_version.is_empty() {
            request = request.header("x-ide-version", &self.ide_version);
        }
        request
    }
}

/// 与 JavaScript encodeURIComponent 相同，保留其 ASCII 字符集并按 UTF-8 字节编码。
fn component(value: &str) -> String {
    use std::fmt::Write;
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to String is infallible");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 空格、逗号和多字节文本必须按官方编码；不能使用表单编码的加号。
    #[test]
    fn native_metadata_uses_uri_component_encoding() {
        assert_eq!(
            component("Mac Book,设备"),
            "Mac%20Book%2C%E8%AE%BE%E5%A4%87"
        );
        assert_eq!(component("-_.!~*'()"), "-_.!~*'()");
        assert_eq!(component("a+b/%"), "a%2Bb%2F%25");
    }

    /// 旧报告没有内核版本时保留未知，不把 GUI 版本误作 IDE 版本。
    #[test]
    fn missing_ide_version_does_not_fabricate_a_native_version() {
        let device: ClientDevice = serde_json::from_value(json!({
            "device_id":"12345","device_brand":"TestMac1,2","device_type":"mac",
            "os_version":"macOS 14.0","app_version":"3.4.24"
        }))
        .unwrap();
        device.validate().unwrap();
        let request = device
            .native_headers(reqwest::Client::new().post("http://127.0.0.1/"))
            .build()
            .unwrap();
        assert!(!request.headers().contains_key("x-ide-version"));
        assert!(!request.headers().contains_key("x-machine-id"));
        assert_eq!(request.headers()["x-os-version"], "macOS%2014.0");
        assert_eq!(request.headers()["x-device-brand"], "TestMac1%2C2");
    }
}
