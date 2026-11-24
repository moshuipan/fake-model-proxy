//! 每日访问官方登录初始化接口，并以当天积分批次的有效期核对保留结果。

use super::{RaccoonBackend, client};
use crate::core::AppError;
use crate::tasks::{TaskKind, TaskOutcome, TaskProvider, TaskStatus, scheduler};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use time::{Date, OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

struct DailyGrant {
    expire_at: OffsetDateTime,
}

/// 解析北京时间自然日；禁止把补跑旧日期当成今天登录。
fn day_start(period: &str) -> Result<i64, AppError> {
    let date = Date::parse(
        period,
        &time::format_description::parse_borrowed::<2>("[year]-[month]-[day]").unwrap(),
    )
    .map_err(|_| AppError::bad_request("小浣熊登录任务日期无效"))?;
    Ok(date
        .midnight()
        .assume_offset(UtcOffset::from_hms(8, 0, 0).unwrap())
        .unix_timestamp())
}

/// 官方日期必须带时区，缺失或损坏不能作为延期依据。
fn timestamp(value: &Value, field: &str) -> Result<OffsetDateTime, AppError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .and_then(|text| OffsetDateTime::parse(text, &Rfc3339).ok())
        .ok_or_else(|| client::protocol_error("小浣熊每日积分明细缺少有效发放或到期时间"))
}

impl DailyGrant {
    /// 同一批积分的七个自然日包含发放日；官网使用末日 23:59:59。
    fn retained(&self, start: i64) -> bool {
        self.expire_at.unix_timestamp() >= start + 7 * 86_400 - 1
    }

    /// 展示服务器实际到期时间，不从余额或固定奖励数推算。
    fn message(&self) -> String {
        let expires = self
            .expire_at
            .to_offset(UtcOffset::from_hms(8, 0, 0).unwrap());
        format!(
            "已确认今日每日积分有效至 {} {:02}:{:02}:{:02}（北京时间）",
            expires.date(),
            expires.hour(),
            expires.minute(),
            expires.second()
        )
    }
}

impl RaccoonBackend {
    /// 最多读取一百条实际账单；分页异常或未找到当天批次均不推测成功。
    async fn daily_grant(
        &self,
        account_id: &str,
        start: i64,
    ) -> Result<Option<DailyGrant>, AppError> {
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        for _ in 0..5 {
            let mut url = reqwest::Url::parse(client::ORIGIN)
                .unwrap()
                .join(client::BILLS_PATH)
                .unwrap();
            url.query_pairs_mut()
                .append_pair("paging.limit", "20")
                .append_pair("type", "all");
            if let Some(cursor) = &cursor {
                url.query_pairs_mut().append_pair("cursor", cursor);
            }
            let path = format!("{}?{}", url.path(), url.query().unwrap());
            let data = self.get_json(account_id, &path).await?;
            let items = data
                .get("items")
                .and_then(Value::as_array)
                .ok_or_else(|| client::protocol_error("小浣熊积分账单缺少明细列表"))?;
            for item in items {
                if item.get("biz_type").and_then(Value::as_str) != Some("daily_grant") {
                    continue;
                }
                let granted = timestamp(item, "grant_at")?.unix_timestamp();
                if !(start..start + 86_400).contains(&granted) {
                    continue;
                }
                if item
                    .get("points")
                    .and_then(crate::accounts::quota::value_units)
                    .is_none_or(|points| points <= 0)
                {
                    return Err(client::protocol_error("小浣熊每日积分发放数无效"));
                }
                return Ok(Some(DailyGrant {
                    expire_at: timestamp(item, "expire_at")?,
                }));
            }
            match data
                .get("has_more")
                .filter(|value| !value.is_null())
                .or_else(|| data.pointer("/paging/has_more"))
                .and_then(Value::as_bool)
            {
                Some(false) => return Ok(None),
                Some(true) => {
                    let next = data
                        .get("next_cursor")
                        .filter(|value| !value.is_null())
                        .or_else(|| data.get("cursor"))
                        .filter(|value| !value.is_null())
                        .or_else(|| data.pointer("/paging/next_cursor"))
                        .filter(|value| !value.is_null())
                        .or_else(|| data.pointer("/paging/cursor"))
                        .and_then(Value::as_str)
                        .filter(|cursor| !cursor.is_empty() && cursor.len() <= 4096)
                        .ok_or_else(|| client::protocol_error("小浣熊积分账单分页缺少游标"))?;
                    if !seen.insert(next.to_owned()) {
                        return Err(client::protocol_error("小浣熊积分账单分页未前进"));
                    }
                    cursor = Some(next.to_owned());
                }
                None => return Err(client::protocol_error("小浣熊积分账单缺少分页状态")),
            }
        }
        Ok(None)
    }

    /// 已保留时只查询；否则访问官网初始化接口，再核对当天批次的真实有效期。
    async fn preserve_daily_points(
        &self,
        account_id: &str,
        period: &str,
    ) -> Result<TaskOutcome, AppError> {
        let start = day_start(period)?;
        if scheduler::day_at(scheduler::now()) != period {
            return Err(AppError::bad_request("小浣熊登录任务只能执行北京时间当天"));
        }
        if let Some(grant) = self.daily_grant(account_id, start).await?
            && grant.retained(start)
        {
            if scheduler::day_at(scheduler::now()) != period {
                return Ok(TaskOutcome::unknown("核对期间已跨日，请查看官网积分有效期"));
            }
            return Ok(TaskOutcome {
                status: TaskStatus::AlreadyDone,
                message: grant.message(),
                amount: None,
            });
        }
        if scheduler::day_at(scheduler::now()) != period {
            return Ok(TaskOutcome::unknown("登录前已跨日，请查看官网积分有效期"));
        }
        // 对应官网会话初始化，不重复换票、强制刷新或提交首次登录奖励。
        self.get_json(account_id, client::USER_PATH).await?;
        self.get_json(account_id, client::SETTINGS_PATH).await?;
        let verified = self.daily_grant(account_id, start).await;
        if scheduler::day_at(scheduler::now()) != period {
            return Ok(TaskOutcome::unknown(
                "登录核对期间已跨日，请查看官网积分有效期",
            ));
        }
        match verified {
            Ok(Some(grant)) if grant.retained(start) => {
                Ok(TaskOutcome::succeeded(grant.message(), None))
            }
            Ok(_) => Ok(TaskOutcome::unknown(
                "已访问官网登录接口，但未确认今日积分延期；请打开官网工作台核对",
            )),
            Err(error) => Ok(TaskOutcome::unknown(format!(
                "已访问官网登录接口，但积分有效期核对失败：{}；请打开官网工作台核对",
                error.message()
            ))),
        }
    }
}

#[async_trait]
impl TaskProvider for RaccoonBackend {
    /// 复用每日任务调度；业务行为是登录保留积分。
    fn kind(&self) -> TaskKind {
        TaskKind::Checkin
    }

    /// 任务只能使用所属服务的账号。
    fn backend_id(&self) -> &str {
        &self.record.id
    }

    /// 兼容单账号入口，账号选择沿用已配置的账号池。
    async fn run(&self, period: &str) -> Result<TaskOutcome, AppError> {
        self.preserve_daily_points(&self.select_account()?.id, period)
            .await
    }

    /// 手动、批量和自动任务均针对明确账号执行并共享每日幂等记录。
    async fn run_for_account(
        &self,
        account_id: &str,
        period: &str,
    ) -> Result<TaskOutcome, AppError> {
        self.preserve_daily_points(account_id, period).await
    }
}
