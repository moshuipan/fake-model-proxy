//! WorkBuddy 的目录积分报价；仅应用匹配模型且当前有效的活动。

use crate::backend::catalog::{credit_multiplier, numeric_factor};
use serde_json::Value;
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

/// 当前有效报价及活动有效期。
pub(super) struct Quote {
    pub factor: Option<f64>,
    pub original: Option<f64>,
    pub valid_until: Option<u64>,
}

/// 活动时间规则的判定结果，未知不能等同于有效。
enum Window {
    Active(Option<u64>),
    Inactive(Option<u64>),
    Unknown,
}

/// 读取 credits，并按活动优先级计算当前有效倍率。
pub(super) fn quote(model: &Value, promotions: &[Value], now: u64) -> Quote {
    let base = model.get("credits").and_then(credit_multiplier);
    let plain = || Quote {
        factor: base,
        original: None,
        valid_until: None,
    };
    let Some(id) = model.get("id").and_then(Value::as_str) else {
        return plain();
    };
    let mut candidates = promotions
        .iter()
        .map(|promotion| (promotion, false))
        .chain(
            model
                .get("promotions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|promotion| (promotion, true)),
        )
        .filter(|(promotion, local)| {
            let targeted = promotion
                .get("modelIds")
                .and_then(Value::as_array)
                .map_or(*local, |models| {
                    models.iter().any(|model| model.as_str() == Some(id))
                });
            targeted
                && promotion.get("enabled").and_then(Value::as_bool) != Some(false)
                && promotion.get("discount").is_some_and(|discount| {
                    discount.get("discountedCredits").is_some() || discount.get("factor").is_some()
                })
        })
        .map(|(promotion, _)| promotion)
        .collect::<Vec<_>>();
    candidates.sort_by_key(|promotion| {
        std::cmp::Reverse(
            promotion
                .get("priority")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        )
    });
    let mut next_change = None;
    for promotion in candidates {
        let valid_until = match active_window(promotion, now) {
            Window::Inactive(next) => {
                next_change = earliest(next_change, next);
                continue;
            }
            Window::Unknown => {
                return Quote {
                    factor: None,
                    original: base,
                    valid_until: None,
                };
            }
            Window::Active(until) => earliest(next_change, until),
        };
        let discount = &promotion["discount"];
        let explicit = discount
            .get("discountedCredits")
            .and_then(credit_multiplier);
        let calculated = base
            .zip(discount.get("factor").and_then(numeric_factor))
            .and_then(|(base, factor)| {
                let value = base * factor;
                if !value.is_finite() {
                    return None;
                }
                // 只消除浮点乘法的尾差；小倍率不能被舍入成免费。
                let rounded = (value * 1e12).round() / 1e12;
                Some(if !rounded.is_finite() || (rounded == 0.0 && value > 0.0) {
                    value
                } else {
                    rounded
                })
            });
        let factor = explicit.or(calculated);
        return Quote {
            factor,
            original: base,
            valid_until,
        };
    }
    Quote {
        valid_until: next_change,
        ..plain()
    }
}

/// 合并价格可能变化的边界，采用最早的已知时刻。
fn earliest(first: Option<u64>, second: Option<u64>) -> Option<u64> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        _ => first.or(second),
    }
}

/// 解析 RFC3339 时间，不接受无法识别或早于 Unix epoch 的时间。
fn timestamp(value: &Value) -> Option<u64> {
    let time = OffsetDateTime::parse(value.as_str()?, &Rfc3339).ok()?;
    u64::try_from(time.unix_timestamp()).ok()
}

/// 校验有效起止与每日时间窗，返回当前报价的最早失效点。
fn active_window(promotion: &Value, now: u64) -> Window {
    let Some(schedule) = promotion.get("schedule") else {
        return Window::Active(None);
    };
    let Some(schedule) = schedule.as_object() else {
        return Window::Unknown;
    };
    let mut until = None;
    if let Some(start) = schedule.get("validFrom") {
        let Some(start) = timestamp(start) else {
            return Window::Unknown;
        };
        if now < start {
            return Window::Inactive(Some(start));
        }
    }
    if let Some(end) = schedule.get("validUntil") {
        let Some(end) = timestamp(end) else {
            return Window::Unknown;
        };
        if now >= end {
            return Window::Inactive(None);
        }
        until = Some(end);
    }
    if schedule.contains_key("weekdays") || schedule.contains_key("weekly") {
        return Window::Unknown;
    }
    let Some(daily) = schedule.get("daily") else {
        return Window::Active(until);
    };
    let Some(daily) = daily.as_array() else {
        return Window::Unknown;
    };
    let offset = match schedule.get("timezone").and_then(Value::as_str) {
        Some("Asia/Shanghai" | "Asia/Hong_Kong" | "Asia/Singapore") => {
            UtcOffset::from_hms(8, 0, 0).ok()
        }
        Some("UTC" | "Etc/UTC") => Some(UtcOffset::UTC),
        Some("Asia/Tokyo") => UtcOffset::from_hms(9, 0, 0).ok(),
        _ => None,
    };
    let Some(offset) = offset else {
        return Window::Unknown;
    };
    let Some(local) = i64::try_from(now)
        .ok()
        .and_then(|now| OffsetDateTime::from_unix_timestamp(now).ok())
        .map(|now| now.to_offset(offset))
    else {
        return Window::Unknown;
    };
    let minute = u32::from(local.hour()) * 60 + u32::from(local.minute());
    let day_start = local
        .date()
        .midnight()
        .assume_offset(offset)
        .unix_timestamp();
    let mut next_start = None;
    for window in daily {
        let Some(start) = window.get("start").and_then(minutes) else {
            return Window::Unknown;
        };
        let Some(end) = window.get("end").and_then(minutes) else {
            return Window::Unknown;
        };
        let active = if start == end {
            true
        } else if start < end {
            minute >= start && minute < end
        } else {
            minute >= start || minute < end
        };
        if active {
            let next_day = start >= end && (start == end || minute >= start);
            let expires = day_start + i64::from(end) * 60 + if next_day { 86_400 } else { 0 };
            let Some(expires) = u64::try_from(expires).ok() else {
                return Window::Unknown;
            };
            return Window::Active(Some(until.map_or(expires, |until| until.min(expires))));
        }
        let starts = day_start + i64::from(start) * 60 + if minute >= start { 86_400 } else { 0 };
        let Some(starts) = u64::try_from(starts).ok() else {
            return Window::Unknown;
        };
        if until.is_none_or(|end| starts < end) {
            next_start = earliest(next_start, Some(starts));
        }
    }
    Window::Inactive(next_start)
}

/// 解析 H:MM / HH:MM 形式的分钟数，并拒绝非法时间。
fn minutes(value: &Value) -> Option<u32> {
    let (hour, minute) = value.as_str()?.split_once(':')?;
    let (hour, minute) = (hour.parse::<u32>().ok()?, minute.parse::<u32>().ok()?);
    (hour < 24 && minute < 60).then_some(hour * 60 + minute)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 构造确定的测试时间，避免活动测试依赖机器当前时间。
    fn at(time: &str) -> u64 {
        timestamp(&json!(time)).unwrap()
    }

    /// 普通数值金额不能误当倍率，明确倍率中的零值需要保留。
    #[test]
    fn explicit_credits_and_unknown_are_distinct() {
        for (credits, expected) in [
            (json!("x0.15"), Some(0.15)),
            (json!("0x"), Some(0.0)),
            (json!(0.2), None),
            (json!("2 credits"), None),
        ] {
            assert_eq!(
                quote(&json!({"id":"m","credits":credits}), &[], 0).factor,
                expected
            );
        }
    }

    /// 跨午夜活动只在指定时区和时间窗内生效，白天回到基本倍率。
    #[test]
    fn daily_discount_respects_timezone_and_boundary() {
        let model = json!({"id":"glm-5.2","credits":"x0.79"});
        let promotions = [
            json!({"modelIds":["glm-5.2"],"discount":{"discountedCredits":"0.50x","factor":0.5},
            "schedule":{"timezone":"Asia/Shanghai","daily":[{"start":"23:00","end":"7:50"}]}}),
        ];
        let night = quote(&model, &promotions, at("2026-11-19T02:00:00+08:00"));
        assert_eq!(night.factor, Some(0.5));
        assert_eq!(night.original, Some(0.79));
        assert_eq!(night.valid_until, Some(at("2026-11-19T07:50:00+08:00")));
        let day = quote(&model, &promotions, at("2026-11-19T07:50:00+08:00"));
        assert_eq!(day.factor, Some(0.79));
        assert!(day.original.is_none());
        assert_eq!(day.valid_until, Some(at("2026-11-19T23:00:00+08:00")));
    }

    /// 未来、过期及禁用活动不能改变当前价格，当前有效活动可以明确免费。
    #[test]
    fn promotion_dates_and_disabled_state_are_respected() {
        let model = json!({"id":"m","credits":"x0.29"});
        let promotion = json!({"modelIds":["m"],"discount":{"discountedCredits":"0x"},
            "schedule":{"validFrom":"2026-11-16T00:00:00+08:00","validUntil":"2026-12-01T00:00:00+08:00"}});
        assert_eq!(
            quote(
                &model,
                std::slice::from_ref(&promotion),
                at("2026-11-15T23:59:59+08:00")
            )
            .factor,
            Some(0.29)
        );
        assert_eq!(
            quote(
                &model,
                std::slice::from_ref(&promotion),
                at("2026-11-19T12:00:00+08:00")
            )
            .factor,
            Some(0.0)
        );
        assert_eq!(
            quote(
                &model,
                std::slice::from_ref(&promotion),
                at("2026-12-01T00:00:00+08:00")
            )
            .factor,
            Some(0.29)
        );
        let mut disabled = promotion;
        disabled["enabled"] = json!(false);
        assert_eq!(
            quote(&model, &[disabled], at("2026-11-19T12:00:00+08:00")).factor,
            Some(0.29)
        );
    }

    /// 只应用匹配模型的最高优先级有效活动，因子计算不能覆盖显式价格。
    #[test]
    fn promotion_target_priority_and_factor_are_preserved() {
        let model = json!({"id":"m","credits":"x0.2"});
        let promotions = [
            json!({"modelIds":["other"],"priority":999,"discount":{"discountedCredits":"0x"}}),
            json!({"modelIds":["m"],"priority":10,"discount":{"factor":0.5}}),
            json!({"modelIds":["m"],"priority":20,"discount":{"factor":0.2,"discountedCredits":"0.03x"}}),
        ];
        assert_eq!(quote(&model, &promotions, 0).factor, Some(0.03));
        assert_eq!(quote(&model, &promotions[1..2], 0).factor, Some(0.1));
    }

    /// 无法解析活动时间规则时保持当前倍率未知，不能误报免费。
    #[test]
    fn invalid_schedule_is_not_assumed_active() {
        let model = json!({"id":"m","credits":"x1.0"});
        let promotion = json!({"modelIds":["m"],"discount":{"discountedCredits":"0x"},"schedule":{"validUntil":"bad-date"}});
        assert!(quote(&model, &[promotion], 0).factor.is_none());
    }

    /// 全局无目标活动不能作用于所有模型，模型内活动则允许省略目标列表。
    #[test]
    fn untargeted_root_promotions_do_not_apply_globally() {
        let promotion = json!({"discount":{"discountedCredits":"0x"}});
        let model = json!({"id":"m","credits":"x0.29"});
        assert_eq!(
            quote(&model, std::slice::from_ref(&promotion), 0).factor,
            Some(0.29)
        );
        let model = json!({"id":"m","credits":"x0.29","promotions":[promotion]});
        assert_eq!(quote(&model, &[], 0).factor, Some(0.0));
    }

    /// 即将开始的高优先级活动会使现有报价失效，未来免费不能提前套用。
    #[test]
    fn future_higher_priority_discount_expires_current_quote() {
        let model = json!({"id":"m","credits":"x1"});
        let promotions = [
            json!({"modelIds":["m"],"priority":20,"discount":{"discountedCredits":"0x"},
                "schedule":{"validFrom":"2026-11-20T00:00:00+08:00"}}),
            json!({"modelIds":["m"],"priority":10,"discount":{"factor":0.5}}),
        ];
        let quote = quote(&model, &promotions, at("2026-11-19T12:00:00+08:00"));
        assert_eq!(quote.factor, Some(0.5));
        assert_eq!(quote.valid_until, Some(at("2026-11-20T00:00:00+08:00")));
    }
}
