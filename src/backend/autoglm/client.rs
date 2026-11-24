//! AutoGLM 官方客户端的双域、签名及账号接口。

use super::credential::{Credential, Region};
use crate::core::{AppError, UpstreamError};
use crate::store::BackendRecord;
use reqwest::{Method, RequestBuilder, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

pub const CN_CHAT: &str = "https://autoglm-acceleration-api.zhipuai.cn";
pub const CN_ACCOUNT: &str = "https://autoglm-api.zhipuai.cn";
pub const GLOBAL_BASE: &str = "https://autoglm-api.autoglm.ai";
/// 厂商协议路径和身份字段必须保持原值，不能随控制台名称改变。
pub const CHAT_PATH: &str = "/autoclaw-proxy/proxy/autoclaw/chat/completions";
pub const MODELS_PATH: &str = "/autoclaw-proxy/proxy/autoclaw-model-config";
pub const TASKS_PATH: &str = "/autoclaw-proxy/proxy/autoclaw-task-list?lang=zh-CN";
pub const CLAIM_PATH: &str = "/autoclaw-proxy/proxy/autoclaw-task-complete";
pub const CHECKIN_CARD_PATH: &str = "/asset-operation/api/v1/checkin/card";
pub const CHECKIN_CLAIM_PATH: &str = "/asset-operation/api/v1/checkin/claim";
pub const WALLET_PATH: &str = "/agent-assetmgr/api/v2/wallets?biz_app_id=autoclaw";

/// 公开客户端协议参数，非用户密钥；部署可通过环境变量更新签名参数。
const APP_ID: &str = "100003";
const APP_KEY: &str = "38d2391985e2369a5fb8227d8e6cd5e5";

/// 配置覆盖保留接口升级及本地假上游验证能力。
#[derive(Clone, Default, Deserialize, schemars::JsonSchema)]
pub(crate) struct Options {
    #[serde(default)]
    region: Option<Region>,
    #[serde(default)]
    client_version: Option<String>,
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    account_base: Option<String>,
    #[serde(default)]
    checkin_protocol: Option<CheckinProtocol>,
}

/// 国内 2.5.3 使用 card；未确认的国际版本保留 task，显式选项支持受控迁移。
#[derive(Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckinProtocol {
    Card,
    Task,
}

/// 同一客户端同时处理登录、目录与聊天；响应正文不进入错误日志。
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    options: Options,
    custom_chat_base: Option<String>,
}

impl Client {
    /// 解析选项并校验地址，忽略预设的官方聊天域以便按账号区域分流。
    pub fn new(record: &BackendRecord) -> Result<Self, AppError> {
        Self::new_with_timeout(record, Duration::from_secs(10))
    }

    /// 后端工厂使用网关连接时限，其他调用方保持默认十秒。
    pub fn new_with_timeout(record: &BackendRecord, timeout: Duration) -> Result<Self, AppError> {
        let options: Options = if record.options.trim().is_empty() {
            Options::default()
        } else {
            serde_json::from_str(&record.options)
                .map_err(|_| AppError::bad_request("AutoGLM 后端选项格式不正确"))?
        };
        let base = record.base_url.trim().trim_end_matches('/');
        let official = [CN_CHAT, CN_ACCOUNT, GLOBAL_BASE].iter().any(|host| {
            base == *host
                || base
                    .strip_prefix(host)
                    .is_some_and(|path| path.starts_with('/'))
        });
        let custom_chat_base = (!base.is_empty() && !official).then(|| base.to_string());
        for base in custom_chat_base.iter().chain(options.account_base.iter()) {
            let url = reqwest::Url::parse(base)
                .map_err(|_| AppError::bad_request("AutoGLM 接口地址无效"))?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Err(AppError::bad_request(
                    "AutoGLM 接口地址必须使用 HTTP 或 HTTPS",
                ));
            }
        }
        let http = reqwest::Client::builder()
            .connect_timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            options,
            custom_chat_base,
        })
    }

    /// 显式后端区域优先，缺省由凭证 JWT 决定。
    pub fn region(&self, credential: &Credential) -> Region {
        self.options
            .region
            .or(credential.region)
            .unwrap_or(Region::Cn)
    }

    /// 默认或国内区域使用国内短信登录，显式国际区域使用海外 OAuth。
    pub fn uses_domestic_login(&self) -> bool {
        self.options.region != Some(Region::Global)
    }

    /// 海外 OAuth 令牌不能写入显式指定国内域的服务。
    pub fn supports_oauth(&self) -> bool {
        self.options.region != Some(Region::Cn)
    }

    /// 国内网页登录沿用当前官方短信协议；显式版本覆盖仍然生效。
    pub fn for_browser_sms(&self) -> Self {
        let mut client = self.clone();
        client.options.region = Some(Region::Cn);
        if client.options.client_version.is_none() {
            client.options.client_version = Some("2.5.3".into());
        }
        client
    }

    /// 读取短信验证配置，未知结构拒绝发码，避免绕过上游验证要求。
    pub async fn sms_captcha_config(&self, device_id: &str) -> Result<Value, AppError> {
        let identity = Credential {
            device_id: device_id.into(),
            region: Some(Region::Cn),
            ..Default::default()
        };
        let config = self
            .auth_post(
                "/userapi/v1/agent-send-code-captcha-config",
                &identity,
                json!({}),
            )
            .await?;
        if config["enabled"].as_bool() == Some(false) {
            return Ok(config);
        }
        if config["enabled"].as_bool() != Some(true)
            || config["captcha_supplier"].as_str() != Some("aliyun")
            || !matches!(config["region"].as_str(), Some("cn" | "sgp"))
            || ["prefix", "scene_id"].iter().any(|key| {
                config[*key]
                    .as_str()
                    .is_none_or(|value| value.trim().is_empty())
            })
        {
            return Err(protocol_error("AutoGLM 短信安全验证配置已变化，请更新代理"));
        }
        Ok(config)
    }

    /// 发码必须由上游明确确认成功，失败不会解锁验证码登录。
    pub async fn send_browser_code(
        &self,
        device_id: &str,
        phone: &str,
        proof: Option<&str>,
    ) -> Result<(), AppError> {
        let identity = Credential {
            device_id: device_id.into(),
            region: Some(Region::Cn),
            ..Default::default()
        };
        let mut body = json!({"phone":phone});
        if let Some(proof) = proof {
            body["ali_captcha_verify_param"] = json!(proof);
        }
        let data = self
            .auth_post("/userapi/v1/agent-send-code", &identity, body)
            .await?;
        if data["result"].as_bool() != Some(true) {
            return Err(protocol_error("AutoGLM 未确认短信发送成功，请重试"));
        }
        Ok(())
    }

    /// 用短信验证码交换完整国内凭证，并沿用发码时的设备身份。
    pub async fn browser_login(
        &self,
        device_id: &str,
        phone: &str,
        code: u64,
    ) -> Result<Credential, AppError> {
        let identity = Credential {
            device_id: device_id.into(),
            region: Some(Region::Cn),
            ..Default::default()
        };
        let data = self
            .auth_post(
                "/userapi/v1/agent-login",
                &identity,
                json!({"phone":phone,"code":code}),
            )
            .await?;
        let mut credential = self.token_response(&data, &identity)?;
        if credential.user_id.trim().is_empty() || credential.refresh_token.trim().is_empty() {
            return Err(protocol_error("AutoGLM 登录响应缺少账号或刷新令牌"));
        }
        credential.region = Some(Region::Cn);
        Ok(credential)
    }

    /// OAuth 使用海外账号域，与国内短信设备和区域隔离。
    async fn oauth_post(
        &self,
        provider: super::oauth::Provider,
        action: &str,
        credential: &Credential,
        body: Value,
    ) -> Result<Value, AppError> {
        let mut client = self.clone();
        client.options.region = Some(Region::Global);
        let mut body = body;
        body["source_id"] = json!("autoclaw");
        body["device_id"] = json!(credential.device_id);
        let navigate_uri = body["navigate_uri"]
            .as_str()
            .ok_or_else(|| protocol_error("AutoGLM 授权缺少回调地址"))?;
        let origin = reqwest::Url::parse(navigate_uri)
            .map_err(|_| protocol_error("AutoGLM 回调地址无效"))?
            .origin()
            .ascii_serialization();
        let path = format!("/userapi/overseasv1/{}-oauth-{action}", provider.name());
        let envelope = decode(
            client
                .request(Method::POST, &path, credential, true)
                .header("Origin", &origin)
                .header("Referer", &origin)
                .timeout(Duration::from_secs(20))
                .json(&body)
                .send()
                .await?,
        )
        .await?;
        if envelope["code"].as_i64() != Some(0) {
            return Err(protocol_error("AutoGLM 授权接口缺少成功状态"));
        }
        if ["captcha_result", "captchaResult", "biz_result", "bizResult"]
            .iter()
            .any(|key| {
                envelope[*key].as_bool() == Some(false)
                    || envelope["data"][*key].as_bool() == Some(false)
            })
        {
            return Err(AppError::bad_request(
                "AutoGLM 安全验证或账号授权未通过，请重新验证",
            ));
        }
        Ok(envelope["data"].clone())
    }

    /// 获取官方授权页，设备 ID 与回调地址必须沿用到令牌兑换。
    pub async fn oauth_url(
        &self,
        provider: super::oauth::Provider,
        credential: &Credential,
        navigate_uri: &str,
        proof: Option<&str>,
    ) -> Result<String, AppError> {
        let mut body = json!({"navigate_uri":navigate_uri});
        if let Some(proof) = proof {
            body["ali_captcha_verify_param"] = json!(proof);
        }
        let data = self.oauth_post(provider, "url", credential, body).await?;
        data["oauth_url"]
            .as_str()
            .filter(|url| !url.trim().is_empty())
            .map(|url| url.trim().to_string())
            .ok_or_else(|| protocol_error("AutoGLM 响应缺少官方授权地址"))
    }

    /// 读取官方前置验证配置，稳定设备 ID 沿用到 OAuth，公开配置不包含凭证。
    pub async fn oauth_captcha_config(&self, device_id: &str) -> Result<Value, AppError> {
        let mut client = self.clone();
        client.options.region = Some(Region::Global);
        let credential = Credential {
            device_id: device_id.into(),
            region: Some(Region::Global),
            ..Default::default()
        };
        client
            .auth_post(
                "/userapi/overseasv1/oauth-captcha-config",
                &credential,
                json!({}),
            )
            .await
    }

    /// 一次性授权码交换完整登录态，明确标记海外区域以供续期和聊天使用。
    pub async fn exchange_oauth(
        &self,
        provider: super::oauth::Provider,
        previous: &Credential,
        navigate_uri: &str,
        code: &str,
        state: &str,
    ) -> Result<Credential, AppError> {
        let data = self
            .oauth_post(
                provider,
                "login",
                previous,
                json!({"navigate_uri":navigate_uri,"code":code,"state":state}),
            )
            .await?;
        let mut credential = self.token_response(&data, previous)?;
        if credential.user_id.trim().is_empty() || credential.refresh_token.trim().is_empty() {
            return Err(protocol_error("AutoGLM 授权响应缺少账号或刷新令牌"));
        }
        credential.region = Some(Region::Global);
        Ok(credential)
    }

    /// 账号域与聊天加速域分离；自定义账号域用于协议验证和网关升级。
    fn base(&self, credential: &Credential, account: bool) -> &str {
        if account {
            if let Some(base) = &self.options.account_base {
                return base.trim_end_matches('/');
            }
        } else if let Some(base) = &self.custom_chat_base {
            return base;
        }
        match (self.region(credential), account) {
            (Region::Cn, false) => CN_CHAT,
            (Region::Cn, true) => CN_CHAT,
            (Region::Global, _) => GLOBAL_BASE,
        }
    }

    /// 每次请求生成时间签名，并设置网关要求的完整客户端身份头。
    pub fn request(
        &self,
        method: Method,
        path: &str,
        credential: &Credential,
        account: bool,
    ) -> RequestBuilder {
        let timestamp = crate::records::now_seconds().to_string();
        let app_key = std::env::var("FMP_AUTOGLM_APP_KEY").unwrap_or_else(|_| APP_KEY.into());
        let signature = format!(
            "{:x}",
            md5::compute(format!("{APP_ID}&{timestamp}&{app_key}"))
        );
        let global = self.region(credential) == Region::Global;
        let version = self.options.client_version.as_deref().unwrap_or(if global {
            "2.4.0"
        } else {
            "2.5.3"
        });
        let platform = self.options.platform.as_deref().unwrap_or("mac");
        let request_id = uuid::Uuid::new_v4().simple().to_string();
        let mut builder = self
            .http
            .request(method, format!("{}{path}", self.base(credential, account)))
            .header("Content-Type", "application/json")
            .header("Accept", "*/*")
            .header("User-Agent", format!("AutoClaw/{version}"))
            .header("X-Auth-Appid", APP_ID)
            .header("X-Auth-TimeStamp", timestamp)
            .header("X-Auth-Sign", signature)
            .header("X-Tm", platform)
            .header("X-Version", version)
            .header("X-Product", "autoclaw")
            .header("X-Channel", "official")
            .header("X-Lang", "zh-CN")
            .header("X-Client-Type", "pc")
            .header("x_trace_id", "autoclaw-desktop")
            .header("X-Request-Id", &request_id)
            .header("X-Trace-Id", request_id);
        if !credential.access_token.is_empty() {
            builder = builder
                .header("Authorization", credential.authorization())
                .header("X-Authorization", credential.authorization());
        }
        builder
    }

    /// 按参考客户端选择签到协议；失败不自动跨协议领取。
    pub(crate) fn checkin_protocol(&self, credential: &Credential) -> CheckinProtocol {
        self.options
            .checkin_protocol
            .unwrap_or(match self.region(credential) {
                Region::Cn => CheckinProtocol::Card,
                Region::Global => CheckinProtocol::Task,
            })
    }

    /// 账号请求固定携带 source_id 与稳定设备 ID；二者不允许由业务字段覆盖。
    async fn auth_post(
        &self,
        path: &str,
        credential: &Credential,
        mut body: Value,
    ) -> Result<Value, AppError> {
        body["source_id"] = json!("autoclaw");
        body["device_id"] = json!(credential.device_id);
        // 刷新用 body 中的 refresh_token 鉴权；过期访问令牌不能进入该通道。
        let mut identity = credential.clone();
        identity.access_token.clear();
        let response = self
            .request(Method::POST, path, &identity, true)
            .timeout(Duration::from_secs(20))
            .json(&body)
            .send()
            .await?;
        let envelope = decode(response).await?;
        if envelope.get("code").and_then(Value::as_i64) != Some(0) {
            return Err(protocol_error("AutoGLM 登录接口缺少成功状态"));
        }
        Ok(envelope.get("data").cloned().unwrap_or(Value::Null))
    }

    /// 仅在官方明确的 400002 业务码下回落，未知网络结果和无效令牌对不再次提交。
    pub async fn refresh(&self, previous: &Credential) -> Result<Credential, AppError> {
        if previous.refresh_token.is_empty() || previous.device_id.is_empty() {
            return Err(AppError::bad_request(
                "续期需要 refresh_token 和登录时的 device_id，可从完整客户端凭证导入",
            ));
        }
        let body = json!({"refresh_token":previous.refresh_token});
        let data = match self
            .auth_post("/userapi/v1/refresh", previous, body.clone())
            .await
        {
            Err(AppError::Upstream(error)) if error.code == "autoglm_400002" => {
                self.auth_post("/userapi/v1/agent-refresh", previous, body)
                    .await?
            }
            result => result?,
        };
        self.token_response(&data, previous)
    }

    /// 新令牌覆盖旧值，缺少的账号元数据保留；到期时间从新 JWT 重算。
    fn token_response(&self, data: &Value, previous: &Credential) -> Result<Credential, AppError> {
        let token = data
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| protocol_error("AutoGLM 响应缺少 access_token"))?;
        let refresh = data
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| protocol_error("AutoGLM 响应缺少 refresh_token"))?;
        let mut value =
            serde_json::to_value(previous).map_err(|_| protocol_error("凭证编码失败"))?;
        value["access_token"] = json!(token);
        value["expires_at"] = Value::Null;
        value["refresh_token"] = json!(refresh);
        if let Some(user) = data.get("user_id").and_then(Value::as_str) {
            value["user_id"] = json!(user);
        }
        Credential::parse(&value.to_string())
    }
}

/// 解析 HTTP 与业务信封，拒绝把 HTTP 200 内的业务失败当作成功。
pub async fn decode(response: Response) -> Result<Value, AppError> {
    let status = response.status().as_u16();
    let value: Value = response.json().await.map_err(|_| {
        if status >= 400 {
            http_error(status)
        } else {
            protocol_error("AutoGLM 返回非 JSON 响应")
        }
    })?;
    if status >= 400
        || value
            .get("code")
            .is_some_and(|code| code.as_i64() != Some(0))
    {
        let code = value.get("code").and_then(Value::as_i64);
        let message = match code {
            Some(400001) => "AutoGLM 参数缺失或格式不正确",
            Some(400002) => "AutoGLM 客户端签名未通过，请检查客户端版本及签名参数",
            Some(630202) => "短信验证码错误",
            Some(630014) => "请重新加载并完成短信安全验证后再发送验证码",
            Some(429 | 630203) => "操作过于频繁，请稍后重试",
            Some(631014) => "当前地区不支持此操作",
            Some(631002) => {
                "AutoGLM 上游拒绝此请求，请确认客户端版本；浏览器授权需要先完成安全验证"
            }
            Some(631016) => "AutoGLM 登录态已过期，请重新登录",
            _ => "AutoGLM 上游拒绝请求",
        };
        let status = match code {
            Some(429 | 630203) => 429,
            Some(631016) => 401,
            _ if status >= 400 => status,
            _ => 400,
        };
        let mut error = UpstreamError::status(status, message);
        error.code = code
            .map(|code| format!("autoglm_{code}"))
            .unwrap_or_else(|| "autoglm_upstream_error".into());
        return Err(AppError::Upstream(error));
    }
    Ok(value)
}

/// 状态错误只显示状态与说明，不泄漏上游可能回显的令牌或请求正文。
pub fn http_error(status: u16) -> AppError {
    let message = match status {
        401 => "AutoGLM 登录态被上游拒绝（HTTP 401）；自动续期未恢复时请重新登录",
        403 => {
            "AutoGLM 上游禁止访问（HTTP 403），请检查模型权限、客户端版本和区域配置；此状态不能单独证明凭证失效"
        }
        402 => "AutoGLM 积分不足",
        406 => "AutoGLM 拒绝请求，请检查模型、系统提示及客户端版本",
        429 => "AutoGLM 请求过于频繁",
        _ => "AutoGLM 上游请求失败",
    };
    AppError::Upstream(UpstreamError::status(status, message))
}

/// 不合法的上游结构不自动重试，防止掩盖协议升级。
pub fn protocol_error(message: &str) -> AppError {
    AppError::Upstream(UpstreamError {
        status: None,
        code: "autoglm_protocol_error".into(),
        message: message.into(),
        retryable: false,
        execution_unknown: false,
    })
}
