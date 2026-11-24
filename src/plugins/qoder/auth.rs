//! Qoder 授权协议与公共会话的适配。

use crate::{
    accounts::Account,
    backend::qoder::{QoderCredential, QoderRegion, oauth},
    core::AppError,
    store::BackendRecord,
};
use serde_json::{Value, json};
struct Protocol;
super::super::device_auth::device_handlers!(Protocol);

#[async_trait::async_trait]
impl super::super::device_auth::DeviceProtocol for Protocol {
    type Pending = oauth::PendingLogin;
    const ID: &'static str = "qoder";
    /// Qoder 的 PKCE 参数在本地生成，不触发隐式登录。
    async fn start(backend: &BackendRecord) -> Result<(Value, Self::Pending), AppError> {
        let (start, pending) = oauth::start(QoderRegion::from_options(&backend.options)?);
        Ok((
            json!({"login_url":start.login_url,"expires_in":start.expires_in}),
            pending,
        ))
    }
    /// 把真实授权结果转换为账号凭证，普通结果不包含 token。
    async fn poll(
        backend: &BackendRecord,
        pending: &Self::Pending,
    ) -> Result<Option<(Account, Value)>, AppError> {
        let Some(login) = oauth::poll(pending).await? else {
            return Ok(None);
        };
        if login.uid.is_empty() {
            return Err(AppError::bad_request("Qoder 未返回账号身份"));
        }
        let name = if login.name.is_empty() {
            &login.uid
        } else {
            &login.name
        };
        Ok(Some((
            Account {
                id: format!("qoder-{}", login.uid),
                backend_id: backend.id.clone(),
                name: name.clone(),
                credential: serde_json::to_string(&QoderCredential::from_login(&login))
                    .map_err(|_| AppError::bad_request("凭证无法编码"))?,
                enabled: true,
            },
            json!({"uid":login.uid,"name":name,"email":login.email}),
        )))
    }
}
