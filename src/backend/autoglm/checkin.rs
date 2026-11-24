//! AutoClaw 国内 2.5.3 的每日卡片协议；未知领取结果不切换旧协议或再次提交。

use super::{AutoGlmBackend, client, unknown_claim};
use crate::core::AppError;
use crate::tasks::{TaskOutcome, TaskStatus, scheduler};
use reqwest::Method;
use serde_json::{Value, json};
use std::time::Duration;

impl AutoGlmBackend {
    /// 先确认当天可领取卡片，一次动作使用一个幂等键，之后只读取卡片核对状态。
    pub(super) async fn checkin_card(
        &self,
        account_id: &str,
        period: &str,
    ) -> Result<TaskOutcome, AppError> {
        if scheduler::day_at(scheduler::now()) != period {
            return Err(AppError::bad_request("AutoGLM 签到只能执行北京时间当天"));
        }
        let card = self
            .get_json(account_id, client::CHECKIN_CARD_PATH, true)
            .await?;
        let data = card
            .get("data")
            .filter(|data| data.is_object())
            .ok_or_else(|| client::protocol_error("AutoGLM 签到卡片缺少 data"))?;
        if data.get("biz_date").and_then(Value::as_str) != Some(period) {
            return Err(client::protocol_error("AutoGLM 签到卡片日期与任务周期不符"));
        }
        match data.get("card_visible").and_then(Value::as_bool) {
            Some(false) => return Ok(TaskOutcome::skipped("上游未开放今日签到卡片")),
            Some(true) => {}
            None => return Err(client::protocol_error("AutoGLM 签到卡片缺少可见状态")),
        }
        match data.get("claim_status").and_then(Value::as_str) {
            Some("claimed") => return Ok(already_done()),
            Some("processing") => return Ok(TaskOutcome::unknown("上游正在处理今日签到")),
            Some("unclaimed") => {}
            _ => return Err(client::protocol_error("AutoGLM 签到卡片状态已变化")),
        }
        let credential = self.credential(account_id, false, None).await?;
        if scheduler::day_at(scheduler::now()) != period {
            return Ok(TaskOutcome::unknown("领取前已跨日，请在官方客户端核对"));
        }
        let key = uuid::Uuid::new_v4().to_string();
        let response = match self
            .client
            .request(Method::POST, client::CHECKIN_CLAIM_PATH, &credential, true)
            .timeout(Duration::from_secs(15))
            .json(&json!({"client_request_id":key}))
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return Ok(unknown_claim()),
        };
        let value = match client::decode(response).await {
            Ok(value) => value,
            Err(AppError::Upstream(error)) => match error.code.as_str() {
                "autoglm_650301" => return Ok(TaskOutcome::skipped("上游确认今日不可领取")),
                "autoglm_650304" => return Ok(already_done()),
                "autoglm_650302" => return Ok(TaskOutcome::unknown("上游正在处理今日签到")),
                "autoglm_650303" => {
                    return Ok(TaskOutcome::unknown("签到身份暂不可用，请在官方客户端核对"));
                }
                _ if error.status.is_none_or(|status| status >= 500) => {
                    return Ok(unknown_claim());
                }
                _ => return Err(AppError::Upstream(error)),
            },
            Err(_) => return Ok(unknown_claim()),
        };
        // 官方 claim 后再读 card；核对失败不能重放已经提交的领取。
        let _verified = self
            .get_json(account_id, client::CHECKIN_CARD_PATH, true)
            .await;
        if scheduler::day_at(scheduler::now()) != period {
            return Ok(TaskOutcome::unknown(
                "领取核对期间已跨日，请在官方客户端核对",
            ));
        }
        claim_outcome(&value, period)
    }
}

/// 已领取是明确上游状态，不从空 data、余额或固定积分推断。
fn already_done() -> TaskOutcome {
    TaskOutcome {
        status: TaskStatus::AlreadyDone,
        message: "今日已签到".into(),
        amount: None,
    }
}

/// 只接受官方完整成功结果；部分成功保留未知，阻止调度重复发放。
fn claim_outcome(value: &Value, period: &str) -> Result<TaskOutcome, AppError> {
    let Some(data) = value.get("data").filter(|data| data.is_object()) else {
        return Ok(unknown_claim());
    };
    if data.get("biz_date").and_then(Value::as_str) != Some(period) {
        return Ok(unknown_claim());
    }
    match data.get("result").and_then(Value::as_str) {
        Some("partial_success") => Ok(TaskOutcome::unknown(
            "上游确认部分签到奖励成功，请在官方客户端核对；不会自动再次领取",
        )),
        Some("success") => {
            let replayed = data
                .get("items")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    !items.is_empty()
                        && items
                            .iter()
                            .all(|item| item.get("replayed").and_then(Value::as_bool) == Some(true))
                });
            if replayed {
                return Ok(already_done());
            }
            Ok(TaskOutcome::succeeded(
                "每日签到成功",
                data.get("claimed_amount")
                    .and_then(Value::as_i64)
                    .filter(|amount| *amount >= 0),
            ))
        }
        _ => Ok(unknown_claim()),
    }
}
