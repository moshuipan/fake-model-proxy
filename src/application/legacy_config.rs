//! v1、授权和界面写操作到配置服务的兼容适配；没有逐项提交或直接写库。

use super::{
    config_service::{ApplyResult, ConfigService, PendingSecrets, from_account, from_record},
    config_types::{ConfigChange, ConfigDocument, error},
};
use crate::{
    accounts::Account,
    core::{AppError, ModelInfo},
    store::BackendRecord,
};

impl ConfigService<'_> {
    /// v1 空密钥保持已有引用，保留旧交互而不复制秘密。
    pub async fn save_backend(&self, record: &BackendRecord) -> Result<ApplyResult, AppError> {
        self.mutate("legacy.service.save", |document, secrets| {
            merge_backend(document, record, secrets)
        })
        .await
    }

    /// 删除实例同时移除其账号，已分配调用名称永久保留。
    pub async fn delete_backend(&self, id: &str) -> Result<ApplyResult, AppError> {
        self.mutate("legacy.service.delete", |document, _| {
            if !document.services.iter().any(|s| s.id == id) {
                return Err(AppError::NotFound {
                    message: "服务不存在".into(),
                });
            }
            document.merge(ConfigChange {
                remove_services: vec![id.into()],
                ..Default::default()
            })
        })
        .await
    }

    /// 账号配置和秘密引用一起提交；空值不会覆盖已轮换 token。
    pub async fn save_account(&self, account: &Account) -> Result<ApplyResult, AppError> {
        self.mutate("legacy.account.save", |document, secrets| {
            merge_account(document, account, secrets)
        })
        .await
    }

    /// 删除账号只影响配置，不回退其用量与任务历史。
    pub async fn delete_account(&self, id: &str) -> Result<ApplyResult, AppError> {
        self.mutate("legacy.account.delete", |document, _| {
            if !document.accounts.iter().any(|a| a.id == id) {
                return Err(AppError::NotFound {
                    message: "账号不存在或为自动管理的引用账号".into(),
                });
            }
            document.accounts.retain(|a| a.id != id);
            Ok(())
        })
        .await
    }

    /// 整个 v1 文件先转换再校验，一次事务保存，任何错误均不部分导入。
    pub async fn import_bundle(
        &self,
        bundle: &crate::config_bundle::ConfigBundle,
    ) -> Result<ApplyResult, AppError> {
        bundle.validate()?;
        self.mutate("legacy.config.import", |document, secrets| {
            for record in &bundle.backends {
                merge_backend(document, record, secrets)?;
            }
            for account in &bundle.accounts {
                // 单令牌引用账号由服务声明派生，无需重复导入。
                if document.services.iter().any(|s| {
                    s.id == account.backend_id
                        && crate::plugins::builtin::registry()
                            .get(&s.plugin_id)
                            .is_ok_and(|p| p.account_control().managed)
                }) {
                    continue;
                }
                merge_account(document, account, secrets)?;
            }
            document.merge(ConfigChange {
                model_aliases: bundle.model_aliases.clone(),
                gateway: bundle.gateway.clone(),
                ..Default::default()
            })
        })
        .await
    }

    /// 调用名称使用已读旧值作 CAS；冲突不产生配置版本。
    pub async fn rename_alias(
        &self,
        backend: &str,
        model: &str,
        name: &str,
        previous: &str,
    ) -> Result<ApplyResult, AppError> {
        self.mutate("legacy.alias.rename", |document, _| {
            if !document.model_aliases.iter().any(|a| {
                a.backend_id == backend
                    && a.model_id == model
                    && a.is_primary
                    && a.alias == previous
            }) {
                return Err(error("alias_conflict", "调用名称已变化，请重新读取"));
            }
            document.merge(ConfigChange {
                model_aliases: vec![crate::model_names::ModelAlias {
                    alias: name.into(),
                    backend_id: backend.into(),
                    model_id: model.into(),
                    is_primary: true,
                }],
                ..Default::default()
            })
        })
        .await
    }

    /// 目录缓存是运行状态；用户显式同步的候选列表通过配置事务生效。
    pub async fn sync_models(
        &self,
        expected: &BackendRecord,
        models: &[ModelInfo],
    ) -> Result<ApplyResult, AppError> {
        self.state
            .store
            .cache_routing_models(expected, models)
            .await?;
        let result = self
            .mutate("models.sync", |document, _| {
                let service = document
                    .services
                    .iter_mut()
                    .find(|s| s.id == expected.id)
                    .ok_or_else(|| error("service_changed", "服务已删除"))?;
                let previous = from_record(expected, &mut PendingSecrets::new())?;
                if !service.enabled
                    || service.plugin_id != expected.kind
                    || service.config.base_url != expected.base_url
                    || service.config.options != previous.config.options
                {
                    return Err(error("service_changed", "服务已停用或连接配置已变化"));
                }
                service.config.models = models.iter().map(|m| m.id.clone()).collect();
                Ok(())
            })
            .await?;
        if result.changes.is_empty() {
            self.activate().await?;
        }
        Ok(result)
    }
}

/// 将单条 v1 服务合入当前声明，避免空秘密覆盖已存引用。
fn merge_backend(
    document: &mut ConfigDocument,
    record: &BackendRecord,
    secrets: &mut PendingSecrets,
) -> Result<(), AppError> {
    let mut incoming = from_record(record, secrets)?;
    if let Some(old) = document.services.iter().find(|s| s.id == record.id) {
        if record.api_key.is_empty() {
            incoming.credentials.api_key = old.credentials.api_key.clone();
        }
        for (name, reference) in &mut incoming.credentials.headers {
            if secrets
                .get(&reference.secret_ref)
                .is_some_and(String::is_empty)
                && let Some(previous) = old.credentials.headers.get(name)
            {
                secrets.remove(&reference.secret_ref);
                *reference = previous.clone();
            }
        }
    }
    document.merge(ConfigChange {
        services: vec![incoming],
        ..Default::default()
    })
}

/// 账号空凭证沿用原引用；显式凭证更换使用独立新引用。
fn merge_account(
    document: &mut ConfigDocument,
    account: &Account,
    secrets: &mut PendingSecrets,
) -> Result<(), AppError> {
    let mut incoming = from_account(account, secrets);
    if account.credential.is_empty()
        && let Some(old) = document.accounts.iter().find(|a| a.id == account.id)
    {
        secrets.remove(&incoming.credential.secret_ref);
        incoming.credential = old.credential.clone();
    }
    document.merge(ConfigChange {
        accounts: vec![incoming],
        ..Default::default()
    })
}
