//! MiniMax 官方 daily-signin 的领取结果契约，已领取不计作本次发放。

use crate::tasks::{TaskOutcome, TaskStatus};
use serde_json::Value;
use std::collections::HashSet;

/// 官方七日面板要求日期唯一，枚举有效，至多一个今天和一个可领取日。
fn valid_panel(panel: &Value) -> bool {
    if panel
        .get("scene")
        .and_then(Value::as_u64)
        .is_none_or(|scene| scene > 4)
    {
        return false;
    }
    let Some(days) = panel
        .get("days")
        .and_then(Value::as_array)
        .filter(|days| days.len() == 7)
    else {
        return false;
    };
    let mut numbers = HashSet::new();
    let mut today = 0;
    let mut claimable = 0;
    for day in days {
        let Some(number) = day
            .get("day_no")
            .and_then(Value::as_u64)
            .filter(|day| (1..=7).contains(day))
        else {
            return false;
        };
        if !numbers.insert(number)
            || !day
                .get("points")
                .and_then(Value::as_f64)
                .is_some_and(|value| value.is_finite() && value >= 0.0)
            || day.get("bonus_points").is_some_and(|value| {
                !value
                    .as_f64()
                    .is_some_and(|value| value.is_finite() && value >= 0.0)
            })
            || !day
                .get("status")
                .and_then(Value::as_u64)
                .is_some_and(|status| (1..=4).contains(&status))
        {
            return false;
        }
        match day.get("is_today").and_then(Value::as_bool) {
            Some(true) => today += 1,
            Some(false) => {}
            None => return false,
        }
        if day.get("status").and_then(Value::as_u64) == Some(2) {
            claimable += 1;
        }
    }
    today <= 1 && claimable <= 1
}

/// 验证领取身份、日期、积分、期限及面板；坏结果保留未知，避免重复发放。
pub(super) fn claim_outcome(data: &Value) -> TaskOutcome {
    let valid = data
        .get("claim_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
        && data
            .get("day_no")
            .and_then(Value::as_u64)
            .is_some_and(|day| (1..=7).contains(&day))
        && data
            .get("points")
            .and_then(Value::as_f64)
            .is_some_and(|value| value.is_finite() && value >= 0.0)
        && data
            .get("expire_at_ms")
            .and_then(Value::as_f64)
            .is_some_and(f64::is_finite)
        && valid_panel(&data["panel"]);
    if !valid {
        return TaskOutcome::unknown("MiniMax 领取响应结构不完整，结果未确认，请核对后再操作");
    }
    match data.get("claim_result").and_then(Value::as_u64) {
        Some(1) => TaskOutcome::succeeded("MiniMax 签到成功", data["points"].as_i64()),
        Some(2) => already_done(),
        _ => TaskOutcome::unknown("MiniMax 领取结果枚举已变化，请核对后再操作"),
    }
}

/// 明确已领取不再次报告实发奖励。
pub(super) fn already_done() -> TaskOutcome {
    TaskOutcome {
        status: TaskStatus::AlreadyDone,
        message: "MiniMax 今日已签到".into(),
        amount: None,
    }
}
