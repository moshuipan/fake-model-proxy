//! Qoder 账号级活动领取，使用上游 claimStatus 和活动窗口判断是否可领取。

use crate::accounts::Account;
use crate::backend::qoder::{QoderCredential, QoderRegion};
use crate::core::error::AppError;
use crate::tasks::{
    TaskKind, TaskOutcome, TaskProvider, TaskStatus, checkin_protocol_error, decode_checkin,
};
use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;

/// 一个后端的签到执行器；每次调用只能处理指定账号。
pub struct QoderCheckin {
    backend_id: String,
    region: QoderRegion,
    accounts: Vec<Account>,
    client: reqwest::Client,
}

impl QoderCheckin {
    /// 构造有限时且不跟随跳转的账号客户端。
    pub fn new(
        backend_id: String,
        region: QoderRegion,
        accounts: Vec<Account>,
    ) -> Result<Self, AppError> {
        Ok(Self {
            backend_id,
            region,
            accounts,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }

    /// 活动域按区域选择；显式对话端点沿用同主机，供网关与本地协议验证使用。
    fn api_base(&self, credential: &QoderCredential) -> String {
        if let Some(endpoint) = &credential.endpoint
            && let Some((host, _)) = endpoint.split_once("/algo/")
        {
            return host.to_string();
        }
        match self.region {
            QoderRegion::Global => "https://openapi.qoder.sh",
            QoderRegion::Cn => "https://openapi.qoder.com.cn",
        }
        .into()
    }

    /// 查询和领取沿用账号令牌与已验证的活动接口头，不生成额外设备身份。
    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        credential: &QoderCredential,
    ) -> reqwest::RequestBuilder {
        let base = self.api_base(credential);
        self.client
            .request(method, format!("{base}{path}"))
            .header(
                "Authorization",
                format!("Bearer {}", credential.device_token),
            )
            .header("accept", "application/json")
            .header("user-agent", "Qoder")
            .header("cosy-clienttype", "10")
            .header("origin", &base)
            .timeout(Duration::from_secs(20))
    }

    /// 严格读取活动数组，缺失字段或业务失败不能显示成“无活动”。
    async fn list_campaigns(&self, credential: &QoderCredential) -> Result<Vec<Value>, AppError> {
        let response = self
            .request(
                reqwest::Method::GET,
                "/sash/api/v1/me/campaigns",
                credential,
            )
            .send()
            .await?;
        let value = decode_checkin(response, "Qoder").await?;
        value
            .get("data")
            .unwrap_or(&value)
            .get("campaigns")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| checkin_protocol_error("Qoder"))
    }

    /// 空请求体领取一个积分活动，成功必须由 CLAIMED 和实际领取凭据确认。
    async fn claim(
        &self,
        credential: &QoderCredential,
        campaign_id: &str,
    ) -> Result<TaskOutcome, AppError> {
        let mut encoded = reqwest::Url::parse("http://localhost/").expect("fixed URL");
        encoded
            .path_segments_mut()
            .expect("hierarchical URL")
            .push(campaign_id);
        let path = format!(
            "/sash/api/v1/me/campaigns/{}/claim",
            encoded.path().trim_start_matches('/'),
        );
        let response = match self
            .request(reqwest::Method::POST, &path, credential)
            .header("content-length", "0")
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return Ok(unknown_claim()),
        };
        let value = match decode_checkin(response, "Qoder").await {
            Ok(value) => value,
            Err(error) if error.status().as_u16() >= 500 => return Ok(unknown_claim()),
            Err(error) => return Err(error),
        };
        let data = value.get("data").unwrap_or(&value);
        if data.get("status").and_then(Value::as_str) != Some("CLAIMED") {
            return Ok(unknown_claim());
        }
        if data.get("replayed").and_then(Value::as_bool) == Some(true) {
            return Ok(already_done());
        }
        if data
            .get("grantId")
            .and_then(Value::as_str)
            .is_none_or(|id| id.is_empty())
        {
            return Ok(unknown_claim());
        }
        let amount = data
            .get("benefit")
            .filter(|benefit| benefit.get("kind").and_then(Value::as_str) == Some("CREDITS"))
            .and_then(|benefit| benefit.get("amount").and_then(Value::as_i64))
            .filter(|amount| *amount >= 0);
        Ok(TaskOutcome::succeeded(
            amount.map_or_else(
                || "Qoder 活动领取成功，奖励数未返回".into(),
                |amount| format!("Qoder 活动领取成功，获得 {amount} 积分"),
            ),
            amount,
        ))
    }

    /// 只领取可领取的积分活动，详情链接和已领取活动不会提交。
    async fn checkin(&self, account: &Account) -> Result<TaskOutcome, AppError> {
        let credential = QoderCredential::parse(account)?;
        credential.validate()?;
        let campaigns = self.list_campaigns(&credential).await?;
        let mut claimed = false;
        for campaign in &campaigns {
            let action = campaign
                .get("actionType")
                .and_then(Value::as_str)
                .ok_or_else(|| checkin_protocol_error("Qoder"))?;
            if action != "CLAIM_BENEFIT" {
                continue;
            }
            match campaign.get("claimStatus").and_then(Value::as_str) {
                Some("CLAIMABLE") => {
                    let id = campaign
                        .get("campaignId")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| checkin_protocol_error("Qoder"))?;
                    return self.claim(&credential, id).await;
                }
                Some("CLAIMED") => claimed = true,
                Some("DISABLED" | "EXPIRED" | "NOT_STARTED" | "UNAVAILABLE") => {}
                _ => return Err(checkin_protocol_error("Qoder")),
            }
        }
        if claimed {
            return Ok(already_done());
        }
        Ok(TaskOutcome::skipped(
            if self.region == QoderRegion::Global {
                "上游当前未提供可领取积分活动；可在官方客户端确认活动资格与设备身份"
            } else {
                "上游当前未提供可领取积分活动"
            },
        ))
    }
}

#[async_trait]
impl TaskProvider for QoderCheckin {
    /// 声明签到任务。
    fn kind(&self) -> TaskKind {
        TaskKind::Checkin
    }
    /// 返回服务标识。
    fn backend_id(&self) -> &str {
        &self.backend_id
    }
    /// 空配置保持跳过；有账号时必须明确指定，防止跨账号记录串写。
    async fn run(&self, _period: &str) -> Result<TaskOutcome, AppError> {
        if self.accounts.is_empty() {
            return Ok(TaskOutcome::skipped("未配置 Qoder 账号"));
        }
        Err(AppError::bad_request("Qoder 签到需要指定账号"))
    }
    /// 每次只处理当前任务记录的启用账号。
    async fn run_for_account(
        &self,
        account_id: &str,
        _period: &str,
    ) -> Result<TaskOutcome, AppError> {
        let account = self
            .accounts
            .iter()
            .find(|account| {
                account.id == account_id && account.enabled && account.backend_id == self.backend_id
            })
            .ok_or_else(|| AppError::bad_request("Qoder 账号不存在、已停用或不属于此服务"))?;
        self.checkin(account).await
    }
    /// Qoder 活动窗口可跨自然日，每次触发先查上游，已领取窗口不会再次提交。
    fn recheck_completed(&self) -> bool {
        true
    }
}

/// 已领取只描述当前活动窗口，不把奖励数当作本次获得。
fn already_done() -> TaskOutcome {
    TaskOutcome {
        status: TaskStatus::AlreadyDone,
        message: "Qoder 当前积分活动已领取".into(),
        amount: None,
    }
}

/// 领取结果未知时不继续提交其他活动，也不允许自动重放。
fn unknown_claim() -> TaskOutcome {
    TaskOutcome::unknown("Qoder 领取结果未知，请在官方客户端核对；代理不会自动重放")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::tasks::{run_and_record, test_support::MockServer};
    use serde_json::json;

    /// 构造同一后端的独立测试账号，活动接口只指向本地假上游。
    fn account(id: &str, server: &MockServer) -> Account {
        Account {
            id: id.into(),
            name: id.into(),
            backend_id: "q".into(),
            enabled: true,
            credential: json!({"uid":id,"device_token":format!("fake-{id}"),
                "endpoint":format!("{}/algo/chat",server.base)})
            .to_string(),
        }
    }

    /// 模拟当前真实活动字段，包括不可领取的详情链接。
    fn campaigns(status: &str, id: &str) -> Value {
        json!({"campaigns":[
            {"campaignId":"details","actionType":"VIEW_DETAILS","claimStatus":"CLAIMABLE"},
            {"campaignId":id,"actionType":"CLAIM_BENEFIT","claimStatus":status,"benefit":{"kind":"CREDITS","amount":100}}
        ]})
    }

    /// 奖励取实际领取响应，不能用活动列表中的预告数代替。
    fn claimed(id: &str, amount: i64) -> Value {
        json!({"status":"CLAIMED","grantId":id,"replayed":false,"benefit":{"kind":"CREDITS","amount":amount}})
    }

    /// 未配置账号时应跳过而不是报错。
    #[tokio::test]
    async fn skips_without_accounts() {
        let checkin = QoderCheckin::new("q".into(), QoderRegion::Global, Vec::new()).unwrap();
        assert_eq!(
            checkin.run("2026-11-19").await.unwrap().status,
            TaskStatus::Skipped
        );
    }

    /// 显式对话端点派生同主机账号接口。
    #[test]
    fn derives_api_base_from_endpoint() {
        let checkin = QoderCheckin::new("q".into(), QoderRegion::Cn, Vec::new()).unwrap();
        let credential = QoderCredential {
            endpoint: Some("https://gateway.qoder.com.cn/algo/api/v2/sse/x".into()),
            ..Default::default()
        };
        assert_eq!(
            checkin.api_base(&credential),
            "https://gateway.qoder.com.cn"
        );
    }

    /// 已领取的真实字段显示 AlreadyDone，详情活动不会发送领取请求。
    #[tokio::test]
    async fn recognizes_claim_status_and_ignores_detail_actions() {
        let server = MockServer::new(vec![(200, campaigns("CLAIMED", "daily"))]).await;
        let provider =
            QoderCheckin::new("q".into(), QoderRegion::Cn, vec![account("a", &server)]).unwrap();
        let outcome = provider.run_for_account("a", "2026-11-19").await.unwrap();
        assert_eq!(outcome.status, TaskStatus::AlreadyDone);
        assert_eq!(outcome.amount, None);
        assert_eq!(server.calls.lock().unwrap().len(), 1);
    }

    /// 同一天的新活动窗口仍可领取；重复执行只查状态，每个账号使用自身身份。
    #[tokio::test]
    async fn claims_once_per_window_and_uses_the_requested_account() {
        let server = MockServer::new(vec![
            (200, campaigns("CLAIMABLE", "first")),
            (200, claimed("grant-1", 37)),
            (200, campaigns("CLAIMED", "first")),
            (200, campaigns("CLAIMABLE", "next")),
            (200, claimed("grant-2", 41)),
            (200, campaigns("CLAIMED", "next")),
        ])
        .await;
        let provider = QoderCheckin::new(
            "q".into(),
            QoderRegion::Cn,
            vec![account("a", &server), account("b", &server)],
        )
        .unwrap();
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        assert_eq!(
            run_and_record(&store, &provider, "a", "2026-11-19")
                .await
                .unwrap()
                .amount,
            Some(37)
        );
        assert_eq!(
            run_and_record(&store, &provider, "a", "2026-11-19")
                .await
                .unwrap()
                .status,
            TaskStatus::AlreadyDone
        );
        assert_eq!(
            run_and_record(&store, &provider, "a", "2026-11-19")
                .await
                .unwrap()
                .amount,
            Some(41)
        );
        assert_eq!(
            run_and_record(&store, &provider, "b", "2026-11-19")
                .await
                .unwrap()
                .status,
            TaskStatus::AlreadyDone
        );
        let calls = server.calls.lock().unwrap();
        assert_eq!(calls.len(), 6);
        assert!(
            calls[..5]
                .iter()
                .all(|(_, token, _)| token == "Bearer fake-a")
        );
        assert_eq!(calls[5].1, "Bearer fake-b");
        assert_eq!(calls[1].0, "/sash/api/v1/me/campaigns/first/claim");
        assert_eq!(calls[1].2, 0);
        assert_eq!(calls[4].2, 0);
    }

    /// 缺失结构和业务错误必须失败，不伪装成“无可领取活动”。
    #[tokio::test]
    async fn invalid_campaign_responses_do_not_become_skipped() {
        for value in [
            json!({"code":401,"data":{"campaigns":[]}}),
            json!({"code":0,"data":{}}),
            json!({"campaigns":[{"actionType":"CLAIM_BENEFIT","claimStatus":"CLAIMABLE"}]}),
        ] {
            let server = MockServer::new(vec![(200, value)]).await;
            let provider =
                QoderCheckin::new("q".into(), QoderRegion::Cn, vec![account("a", &server)])
                    .unwrap();
            assert!(provider.run_for_account("a", "2026-11-19").await.is_err());
            assert_eq!(server.calls.lock().unwrap().len(), 1);
        }
    }

    /// 领取 5xx 或缺少成功凭据均保持 Unknown，同周期不能自动重放。
    #[tokio::test]
    async fn uncertain_claims_are_not_replayed() {
        for (status, value) in [(500, json!({})), (200, json!({"status":"CLAIMED"}))] {
            let server = MockServer::new(vec![
                (200, campaigns("CLAIMABLE", "daily")),
                (status, value),
            ])
            .await;
            let provider =
                QoderCheckin::new("q".into(), QoderRegion::Cn, vec![account("a", &server)])
                    .unwrap();
            let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
            for _ in 0..2 {
                assert_eq!(
                    run_and_record(&store, &provider, "a", "2026-11-19")
                        .await
                        .unwrap()
                        .status,
                    TaskStatus::Unknown
                );
            }
            assert_eq!(server.calls.lock().unwrap().len(), 2);
        }
    }
}
