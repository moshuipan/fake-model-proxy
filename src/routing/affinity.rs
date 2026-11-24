//! 会话亲和。
//!
//! 同一会话在多候选 auto 下必须粘同一后端与模型，否则上下文会在不同
//! 上游之间漂移：思考签名、缓存引用、上游会话 id 都会失效。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 默认亲和有效期。
pub const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60);

/// 默认最大条目数，防止会话 id 无限增长。
pub const DEFAULT_CAPACITY: usize = 4096;

struct Entry {
    address: String,
    expires_at: Instant,
}

/// 会话到模型地址的绑定表。
pub struct SessionAffinity {
    entries: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
    capacity: usize,
}

impl Default for SessionAffinity {
    /// 使用默认 TTL 与容量。
    fn default() -> Self {
        Self::new(DEFAULT_TTL, DEFAULT_CAPACITY)
    }
}

impl SessionAffinity {
    /// 创建绑定表。
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        SessionAffinity {
            entries: Mutex::new(HashMap::new()),
            ttl,
            capacity: capacity.max(1),
        }
    }

    /// 查询会话绑定。过期条目视为不存在。
    pub fn get(&self, session: &str) -> Option<String> {
        if session.is_empty() {
            return None;
        }
        let Ok(mut entries) = self.entries.lock() else {
            return None;
        };
        let now = Instant::now();
        match entries.get(session) {
            Some(entry) if entry.expires_at > now => Some(entry.address.clone()),
            Some(_) => {
                entries.remove(session);
                None
            }
            None => None,
        }
    }

    /// 绑定会话到模型地址。超过容量时清理过期项，仍满则拒绝新绑定。
    pub fn bind(&self, session: &str, address: &str) {
        if session.is_empty() {
            return;
        }
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        let now = Instant::now();
        if entries.len() >= self.capacity && !entries.contains_key(session) {
            entries.retain(|_, entry| entry.expires_at > now);
            if entries.len() >= self.capacity {
                // 容量已满且都有效：放弃新绑定，优先保持已有会话稳定。
                return;
            }
        }
        entries.insert(
            session.to_string(),
            Entry {
                address: address.to_string(),
                expires_at: now + self.ttl,
            },
        );
    }

    /// 解除某个会话的绑定。会话显式切换模型时调用。
    pub fn unbind(&self, session: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(session);
        }
    }

    /// 当前有效条目数。
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .map(|entries| entries.len())
            .unwrap_or(0)
    }

    /// 是否没有任何绑定。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同会话再次查询应返回同一地址。
    #[test]
    fn binds_and_reads_back() {
        let affinity = SessionAffinity::default();
        affinity.bind("s1", "b/m");
        assert_eq!(affinity.get("s1").as_deref(), Some("b/m"));
        assert_eq!(affinity.get("s2"), None);
    }

    /// 空会话不参与亲和。
    #[test]
    fn ignores_empty_session() {
        let affinity = SessionAffinity::default();
        affinity.bind("", "b/m");
        assert_eq!(affinity.get(""), None);
        assert_eq!(affinity.len(), 0);
    }

    /// 解绑后不再命中。
    #[test]
    fn unbind_clears() {
        let affinity = SessionAffinity::default();
        affinity.bind("s1", "b/m");
        affinity.unbind("s1");
        assert_eq!(affinity.get("s1"), None);
    }

    /// 过期条目视为不存在。
    #[test]
    fn expired_entry_is_ignored() {
        let affinity = SessionAffinity::new(Duration::from_millis(1), 8);
        affinity.bind("s1", "b/m");
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(affinity.get("s1"), None);
    }
}
