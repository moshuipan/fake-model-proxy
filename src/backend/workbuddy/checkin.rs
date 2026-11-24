//! WorkBuddy 账号级签到；先查活动状态，再领取并确认业务结果。

use super::{WorkBuddyCredential, WorkBuddyProfile, billing_headers};
use crate::accounts::Account;
use crate::core::AppError;
use crate::tasks::{
    TaskKind, TaskOutcome, TaskProvider, TaskStatus, checkin_protocol_error, decode_checkin,
};
use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;

/// 每次只处理一个账号；HTTP 地址独立于响应解析，便于本地假上游验证。
pub struct WorkBuddyCheckin {
    backend_id: String,
    profile: WorkBuddyProfile,
    accounts: Vec<Account>,
    client: reqwest::Client,
    base: String,
}

impl WorkBuddyCheckin {
    /// 使用服务所在官方区域，查询与领取均有明确超时且不跟随跳转。
    pub fn new(
        backend_id: String,
        profile: WorkBuddyProfile,
        accounts: Vec<Account>,
    ) -> Result<Self, AppError> {
        Ok(Self {
            backend_id,
            base: profile.region.base().into(),
            profile,
            accounts,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }

    /// 官方计费 POST 沿用账号身份，不在错误消息里回显响应正文。
    async fn post(&self, credential: &WorkBuddyCredential, path: &str) -> Result<Value, AppError> {
        let mut builder = self
            .client
            .post(format!("{}{path}", self.base))
            .timeout(Duration::from_secs(20))
            .body("{}");
        for (name, value) in billing_headers(&self.profile, credential) {
            builder = builder.header(name, value);
        }
        decode_checkin(builder.send().await?, "WorkBuddy").await
    }

    /// 布尔状态必须明确存在，未知结构不能默认为无活动。
    async fn activity(&self, credential: &WorkBuddyCredential) -> Result<(bool, bool), AppError> {
        let value = self
            .post(credential, "/v2/billing/meter/checkin-activity-status")
            .await?;
        let data = value.get("data").unwrap_or(&value);
        let active = data
            .get("active")
            .and_then(Value::as_bool)
            .ok_or_else(|| checkin_protocol_error("WorkBuddy"))?;
        let checked = if active {
            data.get("today_checked_in")
                .and_then(Value::as_bool)
                .ok_or_else(|| checkin_protocol_error("WorkBuddy"))?
        } else {
            false
        };
        Ok((active, checked))
    }

    /// 企业账号跳过个人活动；已领取与无活动分别呈现，不重复提交。
    async fn checkin(&self, account: &Account) -> Result<TaskOutcome, AppError> {
        let credential = WorkBuddyCredential::parse(account)?;
        credential.validate()?;
        if !credential.enterprise_id.is_empty() {
            return Ok(TaskOutcome::skipped("企业账号不参与个人签到活动"));
        }
        let (active, checked) = self.activity(&credential).await?;
        if !active {
            return Ok(TaskOutcome::skipped("WorkBuddy 当前签到活动未开放"));
        }
        if checked {
            return Ok(TaskOutcome {
                status: TaskStatus::AlreadyDone,
                message: "WorkBuddy 今日已签到".into(),
                amount: None,
            });
        }
        let claim = match self
            .post(&credential, "/v2/billing/meter/daily-checkin")
            .await
        {
            Ok(claim) => claim,
            Err(error) if error.status().as_u16() >= 500 => return Ok(unknown_claim()),
            Err(error) => return Err(error),
        };
        let data = claim.get("data").unwrap_or(&claim);
        if data.get("success").and_then(Value::as_bool) == Some(false) {
            return Err(AppError::bad_request("WorkBuddy 未确认签到成功"));
        }
        let amount = data
            .get("credit")
            .and_then(|value| {
                value
                    .as_i64()
                    .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
            })
            .filter(|amount| *amount >= 0);
        // 明确奖励或成功标志可直接确认；缺失时只查询状态，绝不再次提交领取。
        if amount.is_none()
            && data.get("success").and_then(Value::as_bool) != Some(true)
            && data.get("today_checked_in").and_then(Value::as_bool) != Some(true)
            && !matches!(self.activity(&credential).await, Ok((_, true)))
        {
            return Ok(unknown_claim());
        }
        Ok(TaskOutcome::succeeded(
            amount.map_or_else(
                || "WorkBuddy 签到成功，奖励数未返回".into(),
                |amount| format!("WorkBuddy 签到成功，获得 {amount} 积分"),
            ),
            amount,
        ))
    }
}

#[async_trait]
impl TaskProvider for WorkBuddyCheckin {
    /// 声明签到任务。
    fn kind(&self) -> TaskKind {
        TaskKind::Checkin
    }
    /// 返回服务标识。
    fn backend_id(&self) -> &str {
        &self.backend_id
    }
    /// 有账号时必须明确指定，防止把整组领取结果挂到单个账号。
    async fn run(&self, _period: &str) -> Result<TaskOutcome, AppError> {
        if self.accounts.is_empty() {
            return Ok(TaskOutcome::skipped("未配置 WorkBuddy 账号"));
        }
        Err(AppError::bad_request("WorkBuddy 签到需要指定账号"))
    }
    /// 每次只处理当前任务记录对应的启用账号。
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
            .ok_or_else(|| AppError::bad_request("WorkBuddy 账号不存在、已停用或不属于此服务"))?;
        self.checkin(account).await
    }
    /// 上游按站点时区开放每日活动，重复触发先查询状态，不受代理日期缓存阻挡。
    fn recheck_completed(&self) -> bool {
        true
    }
}

/// 领取已提交但结果无法确认时保持未知，不自动重复领取。
fn unknown_claim() -> TaskOutcome {
    TaskOutcome::unknown("WorkBuddy 签到结果未知，请在官方客户端核对；代理不会自动重放")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::tasks::{run_and_record, test_support::MockServer};
    use serde_json::json;

    /// 测试账号使用无效令牌，任何请求都只能进入本地假上游。
    fn account(id: &str) -> Account {
        Account {
            id: id.into(),
            name: id.into(),
            backend_id: "wb".into(),
            enabled: true,
            credential: json!({"uid":id,"access_token":format!("fake-{id}")}).to_string(),
        }
    }

    /// 使用生产请求与解析逻辑，只替换网络地址为本地回环端口。
    fn provider(server: &MockServer, accounts: Vec<Account>) -> WorkBuddyCheckin {
        let mut provider = WorkBuddyCheckin::new(
            "wb".into(),
            super::super::profile_from_options("").unwrap(),
            accounts,
        )
        .unwrap();
        provider.base = server.base.clone();
        provider
    }

    /// 模拟真实的 code/data 信封，而不是顶层活动状态。
    fn status(active: bool, checked: bool) -> Value {
        json!({"code":0,"msg":"OK","data":{"active":active,"today_checked_in":checked}})
    }

    /// data 内的可领取状态应发起一次领取；并发触发不会重复提交。
    #[tokio::test]
    async fn wrapped_status_claims_once_even_with_concurrent_requests() {
        let server = MockServer::new(vec![
            (200, status(true, false)),
            (200, json!({"code":0,"data":{"credit":"100"}})),
            (200, status(true, true)),
        ])
        .await;
        let provider = provider(&server, vec![account("a")]);
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        let (first, second) = tokio::join!(
            run_and_record(&store, &provider, "a", "2026-11-19"),
            run_and_record(&store, &provider, "a", "2026-11-19"),
        );
        assert_eq!(first.unwrap().status, TaskStatus::Succeeded);
        assert_eq!(second.unwrap().status, TaskStatus::AlreadyDone);
        let record = store
            .get_task("wb", "a", "checkin", "2026-11-19")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.amount, Some(100));
        let calls = server.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[1].0, "/v2/billing/meter/daily-checkin");
    }

    /// 每个账号只读取自身状态，已签到和活动未开放必须分开显示。
    #[tokio::test]
    async fn accounts_and_inactive_or_completed_states_are_distinct() {
        let server =
            MockServer::new(vec![(200, status(true, true)), (200, status(false, false))]).await;
        let provider = provider(&server, vec![account("a"), account("b")]);
        assert_eq!(
            provider
                .run_for_account("a", "2026-11-19")
                .await
                .unwrap()
                .status,
            TaskStatus::AlreadyDone
        );
        assert_eq!(
            provider
                .run_for_account("b", "2026-11-19")
                .await
                .unwrap()
                .status,
            TaskStatus::Skipped
        );
        assert!(
            provider
                .run_for_account("missing", "2026-11-19")
                .await
                .is_err()
        );
        let calls = server.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, "Bearer fake-a");
        assert_eq!(calls[1].1, "Bearer fake-b");
    }

    /// HTTP 200 业务失败和缺失状态不能显示“无活动”，也不能执行领取。
    #[tokio::test]
    async fn errors_and_missing_fields_do_not_become_no_activity() {
        for value in [
            json!({"code":401,"data":{"active":true}}),
            json!({"code":0,"data":{}}),
        ] {
            let server = MockServer::new(vec![(200, value)]).await;
            let provider = provider(&server, vec![account("a")]);
            assert!(provider.run_for_account("a", "2026-11-19").await.is_err());
            assert_eq!(server.calls.lock().unwrap().len(), 1);
        }
    }

    /// 领取失败或无法确认结果时保留 Unknown，不重复提交或伪造零奖励。
    #[tokio::test]
    async fn uncertain_claims_are_not_replayed_or_reported_as_zero() {
        let server = MockServer::new(vec![(200, status(true, false)), (500, json!({}))]).await;
        let provider = provider(&server, vec![account("a")]);
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        for _ in 0..2 {
            let result = run_and_record(&store, &provider, "a", "2026-11-19")
                .await
                .unwrap();
            assert_eq!(result.status, TaskStatus::Unknown);
            assert_eq!(result.amount, None);
        }
        assert_eq!(server.calls.lock().unwrap().len(), 2);
    }

    /// 领取响应无奖励时只查询状态确认成功，奖励继续保持未知。
    #[tokio::test]
    async fn missing_credit_is_confirmed_by_status_without_second_claim() {
        let server = MockServer::new(vec![
            (200, status(true, false)),
            (200, json!({"code":0,"data":null})),
            (200, status(true, true)),
        ])
        .await;
        let provider = provider(&server, vec![account("a")]);
        let result = provider.run_for_account("a", "2026-11-19").await.unwrap();
        assert_eq!(result.status, TaskStatus::Succeeded);
        assert_eq!(result.amount, None);
        assert_eq!(server.calls.lock().unwrap().len(), 3);
    }

    /// 代理日期未变化但上游新日活动已开放时仍可领取，每次先查询状态。
    #[tokio::test]
    async fn a_new_upstream_day_is_not_blocked_by_the_proxy_date() {
        let server = MockServer::new(vec![
            (200, status(true, false)),
            (200, json!({"code":0,"data":{"credit":100}})),
            (200, status(true, false)),
            (200, json!({"code":0,"data":{"credit":120}})),
        ])
        .await;
        let provider = provider(&server, vec![account("a")]);
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        assert_eq!(
            run_and_record(&store, &provider, "a", "2026-11-19")
                .await
                .unwrap()
                .amount,
            Some(100)
        );
        assert_eq!(
            run_and_record(&store, &provider, "a", "2026-11-19")
                .await
                .unwrap()
                .amount,
            Some(120)
        );
        assert_eq!(server.calls.lock().unwrap().len(), 4);
    }
}
