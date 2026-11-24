//! MonkeyCode 每日签到：先查状态、完成官方 PoW 验证、领取后更新单账号额度。

use super::{credential::Credential, pow, profile};
use crate::accounts::{Account, quota_refresh, remote::QuotaConfidence};
use crate::core::{AppError, UpstreamError};
use crate::store::{BackendRecord, Store};
use crate::tasks::{TaskKind, TaskOutcome, TaskProvider, TaskStatus};
use async_trait::async_trait;
use reqwest::Method;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const CHECKIN_PATH: &str = "/api/v1/users/wallet/checkin";

/// 账号从当前存储读取，旧执行器不能向已切换区域的账号发送 Cookie。
pub struct MonkeyCodeCheckin {
    record: BackendRecord,
    server: String,
    gateway: String,
    store: crate::plugins::ScopedHostServices,
    quota: Arc<super::MonkeyCodeBackend>,
    client: reqwest::Client,
}

impl MonkeyCodeCheckin {
    /// 沿用官方区域或本机测试端点，不跟随携带凭证的 HTTP 重定向。
    pub fn new(
        record: &BackendRecord,
        store: Store,
        quota_refresh: Arc<quota_refresh::Control>,
    ) -> Result<Self, AppError> {
        let host = crate::plugins::ScopedHostServices::new(
            record,
            &[],
            store,
            Duration::from_secs(10),
            quota_refresh,
        );
        let quota = Arc::new(super::MonkeyCodeBackend::new(record, Vec::new())?);
        Self::new_with_host(record, host, quota)
    }

    /// 签到复用推理实例的额度读取器与宿主。
    pub fn new_with_host(
        record: &BackendRecord,
        host: crate::plugins::ScopedHostServices,
        quota: Arc<super::MonkeyCodeBackend>,
    ) -> Result<Self, AppError> {
        let (server, gateway) = profile(record)?;
        Ok(Self {
            record: record.clone(),
            server,
            gateway,
            store: host,
            quota,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }

    /// 每次校验账号归属、启用状态和服务配置，错误消息不携带凭证。
    async fn account(&self, id: &str) -> Result<(Account, Credential), AppError> {
        let backend = self.store.get_backend(&self.record.id).await?;
        if backend.is_none_or(|backend| {
            !backend.enabled
                || backend.kind != "monkeycode"
                || backend.base_url != self.record.base_url
                || crate::plugins::service::catalog_identity(&backend)
                    != crate::plugins::service::catalog_identity(&self.record)
        }) {
            return Err(AppError::bad_request(
                "MonkeyCode 服务已停用、删除或配置已变更",
            ));
        }
        let account = self
            .store
            .get_account(id)
            .await?
            .filter(|account| account.enabled && account.backend_id == self.record.id)
            .ok_or_else(|| AppError::bad_request("MonkeyCode 账号不存在、已停用或不属于此服务"))?;
        let credential = Credential::parse(&account.credential)?;
        credential.validate_origin(&self.server, &self.gateway)?;
        if credential.cookie.is_empty() {
            return Err(AppError::bad_request(
                "MonkeyCode 签到需要有效登录态，请在控制台重新登录",
            ));
        }
        Ok((account, credential))
    }

    /// 验证码计算期间的停用、区域修改或重新登录在提交领取前生效。
    async fn unchanged(&self, expected: &Account) -> Result<(), AppError> {
        let (current, _) = self.account(&expected.id).await?;
        if current.credential != expected.credential {
            return Err(AppError::bad_request(
                "MonkeyCode 登录态已更新，请重新执行签到",
            ));
        }
        Ok(())
    }

    /// 仅发送 Cookie；计算验证码端点返回裸 JSON，签到端点另行检查业务信封。
    async fn request(
        &self,
        credential: &Credential,
        method: Method,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, AppError> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.server))
            .header("Cookie", &credential.cookie)
            .timeout(timeout);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| protocol_error("MonkeyCode 签到连接失败，无法确认响应"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::Upstream(UpstreamError::status(
                status.as_u16(),
                if status.as_u16() == 401 {
                    "MonkeyCode 登录态已失效，请在控制台重新登录".into()
                } else {
                    format!("MonkeyCode 签到接口返回 HTTP {}", status.as_u16())
                },
            )));
        }
        response
            .json()
            .await
            .map_err(|_| protocol_error("MonkeyCode 签到响应不是有效 JSON"))
    }

    /// checked_in 必须为布尔值且业务码为零，不从缺失字段推断未签到。
    fn checked(value: &Value) -> Result<bool, AppError> {
        match value.get("code") {
            Some(Value::Number(code)) if code.as_i64() == Some(0) => {}
            Some(Value::String(code)) if code == "0" => {}
            Some(Value::Number(_) | Value::String(_)) => {
                return Err(AppError::bad_request("MonkeyCode 签到接口拒绝请求"));
            }
            _ => return Err(protocol_error("MonkeyCode 签到响应缺少有效业务码")),
        }
        value
            .pointer("/data/checked_in")
            .and_then(Value::as_bool)
            .ok_or_else(|| protocol_error("MonkeyCode 签到响应缺少明确状态"))
    }

    /// 状态查询无领取副作用，未知领取后最多再查询一次用于核对。
    async fn status(&self, credential: &Credential) -> Result<bool, AppError> {
        Self::checked(
            &self
                .request(
                    credential,
                    Method::GET,
                    CHECKIN_PATH,
                    None,
                    Duration::from_secs(15),
                )
                .await?,
        )
    }

    /// 完成官方 challenge → PoW → redeem 协议，验证失败不发起领取。
    async fn captcha(
        &self,
        account: &Account,
        credential: &Credential,
    ) -> Result<(String, u64), AppError> {
        let value = self
            .request(
                credential,
                Method::POST,
                "/api/v1/public/captcha/challenge",
                None,
                Duration::from_secs(15),
            )
            .await?;
        let ticket: pow::Ticket = serde_json::from_value(value)
            .map_err(|_| protocol_error("MonkeyCode 验证码质询格式已变化"))?;
        let deadline = pow::deadline(&ticket)?;
        let token = ticket.token.clone();
        let expires = ticket.expires;
        let cancel = self.store.quota_refresh.shutdown.child_token();
        // 异步请求被取消时，spawn_blocking 中的计算也必须停止。
        let _cancel_on_drop = cancel.clone().drop_guard();
        let solutions = tokio::task::spawn_blocking(move || pow::solve(ticket, deadline, cancel))
            .await
            .map_err(|_| protocol_error("MonkeyCode 验证码计算任务中断"))??;
        self.unchanged(account).await?;
        let value = self
            .request(
                credential,
                Method::POST,
                "/api/v1/public/captcha/redeem",
                Some(json!({"token":token,"solutions":solutions})),
                pow::remaining(expires)?.min(Duration::from_secs(15)),
            )
            .await?;
        if value.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(AppError::bad_request("MonkeyCode 验证码校验未通过，请重试"));
        }
        let token = value
            .get("token")
            .and_then(Value::as_str)
            .filter(|token| pow::valid_token(token))
            .ok_or_else(|| protocol_error("MonkeyCode 验证码校验缺少令牌"))?;
        let expires = value
            .get("expires")
            .and_then(Value::as_u64)
            .ok_or_else(|| protocol_error("MonkeyCode 验证码校验缺少有效期"))?;
        pow::remaining(expires)?;
        Ok((token.into(), expires))
    }

    /// 余额更新失败不能覆盖已确认的签到结果，奖励数只采用官方领取响应。
    async fn with_quota(&self, account: &Account, mut outcome: TaskOutcome) -> TaskOutcome {
        let refreshed = self.store.refresh_quota(account, self.quota.as_ref()).await;
        if !matches!(refreshed, Ok(info) if info.confidence == QuotaConfidence::Verified) {
            outcome
                .message
                .push_str("；额度暂未更新，可手动更新账号额度");
        }
        outcome
    }

    /// 查询已签到则跳过验证码；领取只提交一次，不能确认时保留未知结果。
    async fn checkin(&self, id: &str) -> Result<TaskOutcome, AppError> {
        let (account, credential) = self.account(id).await?;
        if self.status(&credential).await? {
            return Ok(self.with_quota(&account, already_done()).await);
        }
        let (token, expires) = self.captcha(&account, &credential).await?;
        self.unchanged(&account).await?;
        let claim = self
            .request(
                &credential,
                Method::POST,
                CHECKIN_PATH,
                Some(json!({"captcha_token":token})),
                pow::remaining(expires)?.min(Duration::from_secs(15)),
            )
            .await
            .and_then(|value| Self::checked(&value));
        let outcome = match claim {
            Ok(true) => TaskOutcome::succeeded("MonkeyCode 签到成功", None),
            // 明确拒绝时可查询是否在其他窗口完成，仍不重复提交。
            Err(error) if error.status().as_u16() < 500 => {
                if !matches!(self.status(&credential).await, Ok(true)) {
                    return Err(error);
                }
                already_done()
            }
            _ => {
                if !matches!(self.status(&credential).await, Ok(true)) {
                    return Ok(TaskOutcome::unknown(
                        "MonkeyCode 签到结果未知，请在官网核对；代理不会重复提交领取",
                    ));
                }
                TaskOutcome::succeeded("MonkeyCode 今日签到已确认", None)
            }
        };
        Ok(self.with_quota(&account, outcome).await)
    }
}

#[async_trait]
impl TaskProvider for MonkeyCodeCheckin {
    /// 接入手动、批量和每日自动签到的统一任务类型。
    fn kind(&self) -> TaskKind {
        TaskKind::Checkin
    }

    /// 返回该执行器所属服务。
    fn backend_id(&self) -> &str {
        &self.record.id
    }

    /// 有账号时要求调用方明确指定，不把多账号领取挂到一个任务记录。
    async fn run(&self, _period: &str) -> Result<TaskOutcome, AppError> {
        if self
            .store
            .list_accounts_for(&self.record.id)
            .await?
            .is_empty()
        {
            return Ok(TaskOutcome::skipped("未配置 MonkeyCode 账号"));
        }
        Err(AppError::bad_request("MonkeyCode 签到需要指定账号"))
    }

    /// 使用当前账号凭证执行一天一次的签到，周期幂等由任务存储保证。
    async fn run_for_account(
        &self,
        account_id: &str,
        _period: &str,
    ) -> Result<TaskOutcome, AppError> {
        self.checkin(account_id).await
    }
}

/// 上游已签到与本次领取成功分别记录，不推测奖励金额。
fn already_done() -> TaskOutcome {
    TaskOutcome {
        status: TaskStatus::AlreadyDone,
        message: "MonkeyCode 今日已签到".into(),
        amount: None,
    }
}

/// 响应无法确认时使用协议错误，消息不回显 Cookie、验证码或响应正文。
fn protocol_error(message: &str) -> AppError {
    AppError::Upstream(UpstreamError::status(502, message))
}
