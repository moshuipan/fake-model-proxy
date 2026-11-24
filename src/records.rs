//! 请求记录。
//!
//! 有界 JSONL 文件保存最近请求；内存只作为查询索引。不存对话正文或凭证。

use crate::core::{FinishReason, Usage};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 默认保留条数。
pub const DEFAULT_CAPACITY: usize = 1000;

/// 一次上游候选尝试，span 是入站请求 span 的直接子节点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<serde_json::Value>,
    pub span_id: String,
    pub parent_span_id: String,
    pub model: String,
    pub offset_ms: u64,
    pub duration_ms: u64,
    pub outcome: String,
    pub upstream_status: Option<u16>,
    pub error: Option<String>,
}

/// 一次请求的记录。只保留定位信息与计数，不含正文。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestRecord {
    pub id: String,
    pub at: u64,
    pub protocol: String,
    /// 客户端请求的模型地址。
    pub requested_model: String,
    /// 实际使用的模型地址；失败时为空。
    pub resolved_model: String,
    pub stream: bool,
    /// HTTP 状态码。
    pub status: u16,
    pub duration_ms: u64,
    pub tools: usize,
    /// 输入字符数，用于粗看上下文规模。
    pub input_chars: usize,
    /// 入站 body 已读取的字节数；拒收时可能只读取了部分请求。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body_bytes: Option<usize>,
    /// 仅正常读到 body EOF 时为 true；旧记录保持未知。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body_complete: Option<bool>,
    /// 客户端声明的 Content-Length，缺失或非法时保持未知。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_content_length: Option<u64>,
    /// 本次请求生效的入站字节上限。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body_limit_bytes: Option<usize>,
    /// 归一后的消息数，包括工具调用和工具结果。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_count: Option<usize>,
    /// 归一后的图片数，包括工具结果中的图片。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_count: Option<usize>,
    /// 图片 URL（包括 data URL）的字节总数，不保存 URL 内容。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url_bytes: Option<usize>,
    /// 无可用候选时的本地拒绝依据，最多 128 项，不保存上游原始消息。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub routing_rejections: Vec<crate::routing::Rejection>,
    /// running / ok / error / cancelled / interrupted。
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub session_id: Option<String>,
    /// 客户端与会话关联依据，旧文件没有该字段时保持未知。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<crate::client_plugins::ClientKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<crate::client_plugins::SessionLink>,
    pub first_event_ms: Option<u64>,
    pub finish_reason: Option<FinishReason>,
    pub usage: Usage,
    /// 流中上游错误状态，与已经发送的 HTTP 状态分开。
    pub upstream_status: Option<u16>,
    pub attempts: Vec<Attempt>,
}

/// 文件持久化的当前状态；写失败可从管理 API 与界面观察。
#[derive(Debug, Clone, Serialize)]
pub struct PersistenceStatus {
    pub enabled: bool,
    pub path: Option<String>,
    pub capacity: usize,
    pub last_error: Option<String>,
    pub recovered_invalid_lines: usize,
}

struct Inner {
    records: VecDeque<RequestRecord>,
    last_error: Option<String>,
}

/// 环形记录缓冲。
pub struct RecordLog {
    inner: Mutex<Inner>,
    capacity: usize,
    path: Option<PathBuf>,
    recovered_invalid_lines: usize,
}

impl Default for RecordLog {
    /// 使用默认容量。
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl RecordLog {
    /// 创建无文件的测试索引；正式启动必须使用 open。
    pub fn new(capacity: usize) -> Self {
        RecordLog {
            inner: Mutex::new(Inner {
                records: VecDeque::new(),
                last_error: None,
            }),
            capacity: capacity.max(1),
            path: None,
            recovered_invalid_lines: 0,
        }
    }

    /// 恢复文件、裁剪旧记录；上次未结束的请求标记为 interrupted。
    pub fn open(path: &Path, capacity: usize) -> io::Result<Self> {
        if capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "record capacity must be positive",
            ));
        }
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let mut records = VecDeque::new();
        let mut invalid = 0;
        match File::open(path) {
            Ok(file) => {
                for line in BufReader::new(file).lines() {
                    let line = line?;
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<RequestRecord>(&line) {
                        Ok(mut record) => {
                            if record.outcome == "running" {
                                record.outcome = "interrupted".into();
                                record.error = Some("process_interrupted".into());
                                for attempt in &mut record.attempts {
                                    if attempt.outcome == "running" {
                                        attempt.outcome = "interrupted".into();
                                    }
                                }
                            }
                            if records.len() == capacity {
                                records.pop_front();
                            }
                            records.push_back(record);
                        }
                        Err(_) => invalid += 1,
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if invalid > 0 {
            tracing::warn!(invalid_lines = invalid, "skipped invalid request records");
        }
        let log = Self {
            inner: Mutex::new(Inner {
                records,
                last_error: None,
            }),
            capacity,
            path: Some(path.to_owned()),
            recovered_invalid_lines: invalid,
        };
        // 启动时确认路径可写；不能落盘时直接报错，不退化为临时保存。
        log.persist(
            &log.inner
                .lock()
                .map_err(|_| io::Error::other("record lock poisoned"))?
                .records,
        )?;
        Ok(log)
    }

    /// 原子替换文件；同一锁覆盖快照与写入，避免并发请求覆盖较新的版本。
    fn persist(&self, records: &VecDeque<RequestRecord>) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let temporary = path.with_extension("jsonl.tmp");
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut writer = BufWriter::new(options.open(&temporary)?);
        for record in records {
            serde_json::to_writer(&mut writer, record)?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;
        // 先关闭临时文件，避免 Windows 在替换时遇到仍打开的写句柄。
        drop(writer);
        std::fs::rename(&temporary, path)?;
        // Unix 可通过目录 fsync 持久化重命名；Windows 不支持普通 File 打开目录。
        #[cfg(unix)]
        {
            let parent = path
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    /// 保存当前快照并公开写入错误；下一次成功写入会解除错误状态。
    fn save(&self, inner: &mut Inner) {
        inner.last_error = self
            .persist(&inner.records)
            .err()
            .map(|error| error.to_string());
        if let Some(error) = &inner.last_error {
            tracing::error!(%error, "request record persistence failed");
        }
    }

    /// 记录一次调用。超出容量时丢弃最旧记录。
    pub fn push(&self, record: RequestRecord) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if inner.records.len() >= self.capacity {
            inner.records.pop_front();
        }
        inner.records.push_back(record);
        self.save(&mut inner);
    }

    /// 更新仍在保留范围内的记录，不让较早请求的迟到终态挤走新请求。
    pub fn update(&self, record: RequestRecord) {
        if let Ok(mut inner) = self.inner.lock()
            && let Some(existing) = inner.records.iter_mut().find(|item| item.id == record.id)
        {
            *existing = record;
            self.save(&mut inner);
        }
    }

    /// 返回生效容量与持久化状态。
    pub fn persistence(&self) -> PersistenceStatus {
        PersistenceStatus {
            enabled: self.path.is_some(),
            path: self.path.as_ref().map(|path| path.display().to_string()),
            capacity: self.capacity,
            last_error: self
                .inner
                .lock()
                .map(|inner| inner.last_error.clone())
                .unwrap_or_else(|_| Some("record lock poisoned".into())),
            recovered_invalid_lines: self.recovered_invalid_lines,
        }
    }

    /// 按时间倒序返回最近记录。
    pub fn list(&self, limit: usize) -> Vec<RequestRecord> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        inner
            .records
            .iter()
            .rev()
            .take(limit.max(1))
            .cloned()
            .collect()
    }

    /// 清空记录。
    pub fn clear(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.records.clear();
            self.save(&mut inner);
        }
    }

    /// 当前记录数。
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.records.len())
            .unwrap_or(0)
    }

    /// 是否没有记录。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 构造记录所需的当前时间。
pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// 生成请求记录 id。
pub fn new_record_id() -> String {
    format!("req_{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一条最小记录。
    fn record(index: u64) -> RequestRecord {
        RequestRecord {
            id: format!("req_{index}"),
            at: index,
            protocol: "chat".to_string(),
            requested_model: "auto".to_string(),
            resolved_model: "b/m".to_string(),
            stream: true,
            status: 200,
            duration_ms: 10,
            tools: 0,
            input_chars: 5,
            outcome: "ok".to_string(),
            error: None,
            ..Default::default()
        }
    }

    /// 超出容量时丢弃最旧记录。
    #[test]
    fn drops_oldest_when_full() {
        let log = RecordLog::new(2);
        log.push(record(1));
        log.push(record(2));
        log.push(record(3));
        let listed = log.list(10);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "req_3");
        assert_eq!(listed[1].id, "req_2");
    }

    /// 列表按时间倒序返回。
    #[test]
    fn lists_newest_first() {
        let log = RecordLog::new(10);
        log.push(record(1));
        log.push(record(2));
        let listed = log.list(10);
        assert_eq!(listed[0].id, "req_2");
    }
}
