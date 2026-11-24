//! TRAE CN 个人版每日签到，复用账号续期与持久任务幂等记录。

use super::{TraePersonalBackend, credential::Credential};
use crate::core::AppError;
use crate::tasks::{TaskKind, TaskOutcome, TaskProvider, TaskStatus, scheduler};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;

const STATUS: &str = "/trae/api/v2/ug/checkin_credits/status";
const CLAIM: &str = "/trae/api/v2/ug/checkin_credits/claim";

struct CheckinStatus {
    enabled: bool,
    checked_in: bool,
}

/// 个人签到仅能处理北京时间当天，禁止补跑旧日期或跨日领取。
fn current_day(period: &str) -> Result<(), AppError> {
    if period != scheduler::day_at(scheduler::now()) {
        return Err(AppError::bad_request("TRAE 个人签到只能执行北京时间当天"));
    }
    Ok(())
}

/// 明确读取业务码；缺失或损坏的成功信封不能视为领取成功。
fn business_code(value: &Value) -> Option<i64> {
    value["code"]
        .as_i64()
        .or_else(|| value["code"].as_str()?.parse().ok())
}

/// 已完成只表示查询状态，不把当日奖励面额记成此次新增积分。
fn already_done() -> TaskOutcome {
    TaskOutcome {
        status: TaskStatus::AlreadyDone,
        message: "TRAE 个人账号今日已签到".into(),
        amount: None,
    }
}

/// 只保存业务码与请求阶段，区分查询和领取拒绝，不保存响应正文或凭证。
fn rejected(code: i64, stage: &str) -> AppError {
    let message = match code {
        9074 => format!("TRAE 个人签到{stage}已限流（业务码 9074），本次停止领取"),
        9095 => "TRAE 设备今日已领取，但本账号未确认签到（业务码 9095）".into(),
        _ => format!("TRAE 个人签到{stage}被拒绝（业务码 {code}）"),
    };
    AppError::bad_request(message)
}

impl TraePersonalBackend {
    /// 每次提交前重读账号、校验身份和日期；只有明确 401 允许续期后再提交一次。
    async fn checkin_request(
        &self,
        id: &str,
        credential: &mut Credential,
        path: &str,
        period: &str,
    ) -> Result<reqwest::Response, AppError> {
        let latest = self.credential(id, false, None).await?;
        if latest.subject != credential.subject {
            return Err(AppError::bad_request("TRAE 个人账号在签到期间已变更"));
        }
        *credential = latest;
        for attempt in 0..2 {
            let account = self.account(id).await?;
            if Credential::parse(&account.credential)?.subject != credential.subject {
                return Err(AppError::bad_request("TRAE 个人账号在签到期间已变更"));
            }
            current_day(period)?;
            let response = self
                .personal_api_request(path, credential)
                .json(&json!({"req_source":1}))
                .timeout(Duration::from_secs(25))
                .send()
                .await
                .map_err(|_| super::error(None, "TRAE 个人签到网络失败，结果未知", true))?;
            if response.status().as_u16() != 401 || attempt == 1 {
                return Ok(response);
            }
            let next = self
                .credential(id, false, Some(&credential.access_token))
                .await?;
            if next.subject != credential.subject {
                return Err(AppError::bad_request("TRAE 个人账号在签到期间已变更"));
            }
            *credential = next;
        }
        unreachable!("two attempts always return a response or error")
    }

    /// 状态接口必须明确返回成功、活动开关和账号签到状态。
    async fn checkin_status(
        &self,
        id: &str,
        credential: &mut Credential,
        period: &str,
    ) -> Result<CheckinStatus, AppError> {
        let response = self.checkin_request(id, credential, STATUS, period).await?;
        if !response.status().is_success() {
            return Err(super::error(
                Some(response.status().as_u16()),
                "TRAE 个人签到状态查询失败",
                false,
            ));
        }
        let value: Value = response
            .json()
            .await
            .map_err(|_| crate::tasks::checkin_protocol_error("TRAE 个人版"))?;
        let code = business_code(&value)
            .ok_or_else(|| crate::tasks::checkin_protocol_error("TRAE 个人版"))?;
        if code != 0 {
            return Err(rejected(code, "状态查询"));
        }
        let enabled = value["enable"]
            .as_bool()
            .ok_or_else(|| crate::tasks::checkin_protocol_error("TRAE 个人版"))?;
        let checked_in = if enabled {
            value["checked_in"]
                .as_bool()
                .ok_or_else(|| crate::tasks::checkin_protocol_error("TRAE 个人版"))?
        } else {
            false
        };
        Ok(CheckinStatus {
            enabled,
            checked_in,
        })
    }

    /// 先查询账号状态，再领取；提交后的不确定结果必须保存为未知，禁止自动重放。
    async fn checkin_account(&self, id: &str, period: &str) -> Result<TaskOutcome, AppError> {
        current_day(period)?;
        let mut credential = self.credential(id, false, None).await?;
        // 此时尚未提交领取；状态查询失败可明确记为失败，不能制造未知领取围栏。
        // 领取后的 9095 核对仍由下方独立分支保留 unknown 语义。
        let status = self
            .checkin_status(id, &mut credential, period)
            .await
            .map_err(|error| match error {
                AppError::Upstream(mut cause) => {
                    cause.execution_unknown = false;
                    AppError::Upstream(cause)
                }
                error => error,
            })?;
        if !status.enabled {
            return Ok(TaskOutcome::skipped("TRAE 个人账号当前未开放签到"));
        }
        if status.checked_in {
            return Ok(already_done());
        }
        let response = match self
            .checkin_request(id, &mut credential, CLAIM, period)
            .await
        {
            Ok(response) => response,
            Err(AppError::Upstream(error)) if error.execution_unknown => {
                return Ok(TaskOutcome::unknown(error.message));
            }
            Err(error) => return Err(error),
        };
        let http = response.status();
        if !http.is_success() {
            if http.is_client_error() && http.as_u16() != 408 {
                return Err(super::error(
                    Some(http.as_u16()),
                    "TRAE 个人签到领取被拒绝",
                    false,
                ));
            }
            return Ok(TaskOutcome::unknown(format!(
                "TRAE 个人签到领取返回 HTTP {}，请在个人客户端核对，禁止重放",
                http.as_u16()
            )));
        }
        let value: Value = match response.json().await {
            Ok(value) => value,
            Err(_) => {
                return Ok(TaskOutcome::unknown(
                    "TRAE 个人签到领取响应不是 JSON，请在个人客户端核对，禁止重放",
                ));
            }
        };
        match business_code(&value) {
            Some(0) => {
                let amount = value["credits"].as_i64().filter(|amount| *amount >= 0);
                Ok(TaskOutcome::succeeded("TRAE 个人版签到成功", amount))
            }
            Some(9095) => match self.checkin_status(id, &mut credential, period).await {
                Ok(status) if status.checked_in => Ok(already_done()),
                Ok(_) => Err(rejected(9095, "积分领取")),
                Err(_) => Ok(TaskOutcome::unknown(
                    "TRAE 设备今日已领取，但无法核对本账号签到状态，请在个人客户端确认",
                )),
            },
            Some(code) => Err(rejected(code, "积分领取")),
            None => Ok(TaskOutcome::unknown(
                "TRAE 个人签到领取响应缺少业务码，请在个人客户端核对，禁止重放",
            )),
        }
    }
}

#[async_trait]
impl TaskProvider for TraePersonalBackend {
    /// 个人版每日积分签到接入统一调度、批量与手动任务。
    fn kind(&self) -> TaskKind {
        TaskKind::Checkin
    }

    /// 任务归属当前个人服务。
    fn backend_id(&self) -> &str {
        &self.record.id
    }

    /// 旧入口选择当前可用账号，执行前仍从 Store 校验状态。
    async fn run(&self, period: &str) -> Result<TaskOutcome, AppError> {
        let account = self
            .accounts
            .select()
            .ok_or_else(|| AppError::bad_request("请先登录 TRAE 个人账号"))?;
        self.checkin_account(&account.id, period).await
    }

    /// 调度器指定账号，不借用同服务其他账号的身份。
    async fn run_for_account(
        &self,
        account_id: &str,
        period: &str,
    ) -> Result<TaskOutcome, AppError> {
        self.checkin_account(account_id, period).await
    }
}
