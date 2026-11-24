//! 路由额度快照。资源整数使用原始单位；小数使用十进制定点，未知不补零。

use super::remote::{QuotaConfidence, QuotaInfo};
use serde::{Deserialize, Serialize};

/// 积分账本最多保留十八位小数，覆盖上游实际返回的微小扣费尾数。
const DECIMAL_PLACES: u32 = 18;
const DECIMAL_FACTOR: i128 = 1_000_000_000_000_000_000;

/// 可被路由使用的余额，金额序列化为十进制字符串。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    pub remaining: Option<String>,
    pub unit: String,
    pub quota_scope_id: String,
    pub valid_until: Option<u64>,
    pub unlimited: bool,
    pub complete: bool,
}

impl QuotaSnapshot {
    /// 从既有明确的整数余额投影；缺少总量不能推断不限量。
    pub fn from_info(info: &QuotaInfo) -> Self {
        if let Some(snapshot) = &info.routing {
            return snapshot.clone();
        }
        let remaining = info.remaining.or_else(|| {
            info.total
                .zip(info.used)
                .and_then(|(total, used)| total.checked_sub(used))
        });
        let valid_until = info.reset_at.as_ref().and_then(|text| {
            time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
                .ok()
                .and_then(|date| u64::try_from(date.unix_timestamp()).ok())
        });
        Self {
            remaining: remaining.map(|value| value.to_string()),
            unit: info.unit.clone().unwrap_or_default(),
            quota_scope_id: format!("{}/{}", info.backend_id, info.account_id),
            valid_until,
            unlimited: info.note.as_deref() == Some("unlimited"),
            complete: info.confidence == QuotaConfidence::Verified,
        }
    }

    /// 返回固定十八位小数的整数比较值，超过精度或无效数据保持未知。
    pub fn amount(&self) -> Option<i128> {
        self.complete.then_some(())?;
        decimal_units(self.remaining.as_deref()?)
    }
}

/// 将十进制金额解析为十八位定点单位；不经浮点数转换。
pub fn decimal_units(text: &str) -> Option<i128> {
    let text = text.trim();
    let (negative, text) = text
        .strip_prefix('-')
        .map_or((false, text), |value| (true, value));
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let fraction = fraction.trim_end_matches('0');
    if whole.is_empty()
        || !whole.bytes().all(|ch| ch.is_ascii_digit())
        || fraction.len() > DECIMAL_PLACES as usize
        || !fraction.bytes().all(|ch| ch.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.parse::<i128>().ok()?.checked_mul(DECIMAL_FACTOR)?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<i128>()
            .ok()?
            .checked_mul(10_i128.pow(DECIMAL_PLACES - fraction.len() as u32))?
    };
    let units = whole.checked_add(fraction)?;
    if negative {
        units.checked_neg()
    } else {
        Some(units)
    }
}

/// 把定点金额还原为十进制字符串。
pub fn decimal_string(units: i128) -> String {
    let sign = if units < 0 { "-" } else { "" };
    let absolute = units.unsigned_abs();
    let whole = absolute / DECIMAL_FACTOR as u128;
    let fraction = format!("{:018}", absolute % DECIMAL_FACTOR as u128);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{fraction}")
    }
}

/// 接受上游十进制数字或数字字符串，拒绝精度不足及无效金额。
pub fn value_units(value: &serde_json::Value) -> Option<i128> {
    match value {
        serde_json::Value::String(text) => decimal_units(text),
        serde_json::Value::Number(number) => decimal_units(&number.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 小数差异保留，超出支持精度的金额不取整或四舍五入。
    #[test]
    fn decimal_amounts_preserve_precision_and_reject_unsupported_values() {
        assert!(decimal_units("0.01").unwrap() < decimal_units("0.02").unwrap());
        assert_eq!(
            decimal_string(decimal_units("123.456789").unwrap()),
            "123.456789"
        );
        assert_eq!(
            decimal_string(decimal_units("181.19999988").unwrap()),
            "181.19999988"
        );
        for value in ["1.0000000000000000001", "NaN", "1e3", "", "3..4"] {
            assert_eq!(decimal_units(value), None);
        }
    }
}
