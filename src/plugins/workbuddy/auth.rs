//! WorkBuddy 授权协议与公共会话的适配。

use crate::{
    accounts::Account,
    backend::workbuddy::{WorkBuddyCredential, oauth, profile_from_options},
    core::AppError,
    store::BackendRecord,
};
use serde_json::{Value, json};
struct Protocol;
super::super::device_auth::device_handlers!(Protocol);

#[async_trait::async_trait]
impl super::super::device_auth::DeviceProtocol for Protocol {
    type Pending = oauth::Pending;
    const ID: &'static str = "workbuddy";
    /// 由服务端签发本次登录 state，客户端画像来自已保存配置。
    async fn start(backend: &BackendRecord) -> Result<(Value, Self::Pending), AppError> {
        let (start, pending) = oauth::start(profile_from_options(&backend.options)?).await?;
        Ok((
            json!({"login_url":start.login_url,"expires_in":start.expires_in}),
            pending,
        ))
    }
    /// 返回独立账号与公开身份，不把旋转令牌写入操作结果。
    async fn poll(
        backend: &BackendRecord,
        pending: &Self::Pending,
    ) -> Result<Option<(Account, Value)>, AppError> {
        let Some(login) = oauth::poll(pending).await? else {
            return Ok(None);
        };
        let name = if login.nickname.is_empty() {
            &login.uid
        } else {
            &login.nickname
        };
        Ok(Some((
            Account {
                id: format!("workbuddy-{}", login.uid),
                backend_id: backend.id.clone(),
                name: name.clone(),
                credential: serde_json::to_string(&WorkBuddyCredential::from_login(&login))
                    .map_err(|_| AppError::bad_request("凭证无法编码"))?,
                enabled: true,
            },
            json!({"uid":login.uid,"name":name,"enterprise_id":login.enterprise_id}),
        )))
    }
}
