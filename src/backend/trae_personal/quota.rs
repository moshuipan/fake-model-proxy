//! 个人版官方通用积分余额；保留小数，不从积分包或签到奖励推算。

use super::{TraePersonalBackend, credential::Credential};
use crate::accounts::{
    quota::{QuotaSnapshot, decimal_string, value_units},
    remote::{QuotaConfidence, QuotaInfo},
};
use crate::core::AppError;
use serde_json::{Value, json};
use std::time::Duration;

const USAGE: &str = "/trae/api/v2/pay/ide_user_ent_usage";

/// 官方余额响应没有业务码；若返回业务码，则必须明确为成功。
fn parse(value: &Value, backend_id: &str, account_id: &str) -> Result<QuotaInfo, AppError> {
    let object = value
        .as_object()
        .ok_or_else(|| super::error(None, "TRAE 个人额度响应不是 JSON 对象", false))?;
    if let Some(code) = object.get("code") {
        let code = code
            .as_i64()
            .or_else(|| code.as_str()?.parse::<i64>().ok())
            .ok_or_else(|| super::error(None, "TRAE 个人额度业务码无效", false))?;
        if code != 0 {
            return Err(super::error(
                None,
                &format!("TRAE 个人额度请求被拒绝（业务码 {code}）"),
                false,
            ));
        }
    }
    let credits = value["is_credits_billing"]
        .as_bool()
        .ok_or_else(|| super::error(None, "TRAE 个人额度响应缺少计费类型", false))?;
    if !credits {
        return Ok(QuotaInfo::unknown(
            backend_id,
            account_id,
            "TRAE 个人账号当前不是积分计费，无法确认积分余额",
        ));
    }
    let units = value_units(&value["total_balance"])
        .filter(|amount| *amount >= 0)
        .ok_or_else(|| super::error(None, "TRAE 个人额度响应缺少有效积分余额", false))?;
    let remaining = decimal_string(units);
    Ok(QuotaInfo {
        backend_id: backend_id.into(),
        account_id: account_id.into(),
        used: None,
        total: None,
        remaining: remaining.parse().ok(),
        unit: Some("积分".into()),
        reset_at: None,
        confidence: QuotaConfidence::Verified,
        note: Some("官方通用积分余额；模型扣费规则尚未验证".into()),
        checked_at: crate::records::now_seconds(),
        routing: Some(QuotaSnapshot {
            remaining: Some(remaining),
            unit: "积分".into(),
            quota_scope_id: format!("{backend_id}/{account_id}"),
            valid_until: None,
            unlimited: false,
            complete: true,
        }),
    })
}

impl TraePersonalBackend {
    /// 每次从 Store 校验个人身份，仅明确 401 可续期后重试一次。
    pub(super) async fn read_quota(&self, id: &str) -> Result<QuotaInfo, AppError> {
        let mut credential = self.credential(id, false, None).await?;
        for attempt in 0..2 {
            let account = self.account(id).await?;
            if Credential::parse(&account.credential)?.subject != credential.subject {
                return Err(AppError::bad_request("TRAE 个人账号在额度查询期间已变更"));
            }
            let response = self
                .personal_api_request(USAGE, &credential)
                .json(&json!({"require_usage":true,"req_source":1}))
                .timeout(Duration::from_secs(25))
                .send()
                .await
                .map_err(|_| super::error(None, "TRAE 个人额度查询网络失败", false))?;
            if response.status().as_u16() == 401 && attempt == 0 {
                let next = self
                    .credential(id, false, Some(&credential.access_token))
                    .await?;
                if next.subject != credential.subject {
                    return Err(AppError::bad_request("TRAE 个人账号在额度查询期间已变更"));
                }
                credential = next;
                continue;
            }
            if !response.status().is_success() {
                return Err(super::error(
                    Some(response.status().as_u16()),
                    "TRAE 个人额度查询失败",
                    false,
                ));
            }
            let value: Value = response
                .json()
                .await
                .map_err(|_| super::error(None, "TRAE 个人额度响应不是 JSON", false))?;
            let account = self.account(id).await?;
            if Credential::parse(&account.credential)?.subject != credential.subject {
                return Err(AppError::bad_request("TRAE 个人账号在额度查询期间已变更"));
            }
            return parse(&value, &self.record.id, id);
        }
        unreachable!("two attempts always return a response or error")
    }
}
