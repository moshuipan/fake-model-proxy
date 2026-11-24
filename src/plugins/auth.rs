//! 授权会话由插件拥有类型与校验规则，宿主只管理生命周期。

use std::{
    any::{Any, TypeId},
    collections::HashMap,
    sync::{Arc, RwLock},
};

/// 插件自行实现授权会话撤销，公共应用代码不判断厂商名称。
#[async_trait::async_trait]
pub trait SessionStore: Any + Send + Sync {
    /// 服务停用、删除或连接身份变更时撤销等待中的授权。
    async fn cancel_service(&self, service_id: &str);
}

struct Entry {
    value: Arc<dyn Any + Send + Sync>,
    lifecycle: Arc<dyn SessionStore>,
}

/// 每个应用独立持有插件会话，不把具体厂商字段放入 AppState。
#[derive(Default)]
pub struct AuthRegistry {
    entries: RwLock<HashMap<TypeId, Entry>>,
}

impl AuthRegistry {
    /// 获取插件专有会话存储；身份、一次性 state 和有效期仍由插件验证。
    pub fn sessions<T: SessionStore + Default>(&self) -> Arc<T> {
        let mut entries = self
            .entries
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = entries.entry(TypeId::of::<T>()).or_insert_with(|| {
            let value = Arc::new(T::default());
            Entry {
                value: value.clone(),
                lifecycle: value,
            }
        });
        entry
            .value
            .clone()
            .downcast::<T>()
            .expect("session type must match registration key")
    }

    /// 释放指定服务的所有等待会话，不影响其他服务。
    pub async fn cancel_service(&self, id: &str) {
        let sessions = self
            .entries
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|entry| entry.lifecycle.clone())
            .collect::<Vec<_>>();
        for store in sessions {
            store.cancel_service(id).await;
        }
    }
}
