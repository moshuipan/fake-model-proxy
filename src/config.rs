//! 启动配置。
//!
//! 只从环境变量读取，保持个人部署的零配置文件启动。

use std::path::PathBuf;

/// 对话请求体默认上限；管理 API 继续使用框架原有的独立限制。
pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

/// 运行时配置。
#[derive(Debug, Clone)]
pub struct Config {
    /// 监听地址，默认 127.0.0.1:8317。
    pub listen: String,
    /// 数据目录，默认 .fakemodelproxy。
    pub data_dir: PathBuf,
    /// 调用 key。为空表示不校验（仅限本地回环）。
    pub api_key: String,
    /// 管理界面 key。为空时复用 api_key。
    pub admin_key: String,
    /// v2 具名权限密钥，JSON 从 FMP_ACCESS_KEYS 加载。
    pub access_keys: Vec<crate::application::access::AccessKey>,
    /// 三种对话协议允许缓冲的最大请求体字节数。
    pub max_request_body_bytes: usize,
    /// 文件中保留的最近请求数量。
    pub record_capacity: usize,
}

impl Default for Config {
    /// 默认配置与零环境变量启动一致，供测试和进程内装配复用。
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8317".into(),
            data_dir: PathBuf::from(".fakemodelproxy"),
            api_key: String::new(),
            admin_key: String::new(),
            access_keys: Vec::new(),
            max_request_body_bytes: DEFAULT_MAX_REQUEST_BODY_BYTES,
            record_capacity: crate::records::DEFAULT_CAPACITY,
        }
    }
}

impl Config {
    /// 从环境变量加载；无效请求体上限阻止启动，避免误用默认值。
    pub fn from_env() -> Result<Self, crate::core::AppError> {
        let listen = std::env::var("FMP_LISTEN").unwrap_or_else(|_| "127.0.0.1:8317".to_string());
        let data_dir = std::env::var("FMP_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(".fakemodelproxy"));
        let api_key = std::env::var("FMP_API_KEY").unwrap_or_default();
        let admin_key = std::env::var("FMP_ADMIN_KEY")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| api_key.clone());
        let access_keys: Vec<crate::application::access::AccessKey> =
            serde_json::from_str(&std::env::var("FMP_ACCESS_KEYS").unwrap_or_else(|_| "[]".into()))
                .map_err(|_| {
                    crate::core::AppError::bad_request("FMP_ACCESS_KEYS 必须是权限密钥 JSON 数组")
                })?;
        crate::application::access::validate_keys(&access_keys, &admin_key)?;
        Ok(Config {
            listen,
            data_dir,
            api_key,
            admin_key,
            access_keys,
            max_request_body_bytes: parse_request_body_limit(
                std::env::var("FMP_MAX_REQUEST_BODY_MB").ok().as_deref(),
            )?,
            record_capacity: parse_record_capacity(
                std::env::var("FMP_RECORD_CAPACITY").ok().as_deref(),
            )?,
        })
    }

    /// 数据库文件路径。
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("fakemodelproxy.db")
    }

    /// 请求审计文件与其他持久数据共用数据目录。
    pub fn records_path(&self) -> PathBuf {
        self.data_dir.join("requests.jsonl")
    }

    /// 是否启用调用鉴权。
    pub fn requires_api_key(&self) -> bool {
        !self.api_key.is_empty()
    }

    /// 判断是否为回环监听地址。非鉴权模式只允许回环。
    pub fn is_loopback(&self) -> bool {
        let host = self.listen.rsplit(':').nth(1).unwrap_or(&self.listen);
        matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1")
    }
}

/// 保留数量只接受正整数，避免错误配置静默清空审计历史。
fn parse_record_capacity(value: Option<&str>) -> Result<usize, crate::core::AppError> {
    match value {
        None => Ok(crate::records::DEFAULT_CAPACITY),
        Some(value) => value
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                crate::core::AppError::bad_request("FMP_RECORD_CAPACITY 必须是有效的正整数")
            }),
    }
}

/// MiB 配置只接受可表示为字节数的正整数，不允许零值关闭内存边界。
fn parse_request_body_limit(value: Option<&str>) -> Result<usize, crate::core::AppError> {
    let Some(value) = value else {
        return Ok(DEFAULT_MAX_REQUEST_BODY_BYTES);
    };
    value
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .and_then(|value| value.checked_mul(1024 * 1024))
        .ok_or_else(|| {
            crate::core::AppError::bad_request(
                "FMP_MAX_REQUEST_BODY_MB 必须是有效的正整数，单位为 MiB",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 数据库文件必须落在数据目录内。
    #[test]
    fn database_path_is_inside_data_dir() {
        let config = Config {
            listen: "127.0.0.1:8317".into(),
            data_dir: PathBuf::from("/tmp/fmp"),
            api_key: String::new(),
            admin_key: String::new(),
            ..Config::default()
        };
        assert_eq!(
            config.database_path(),
            PathBuf::from("/tmp/fmp/fakemodelproxy.db")
        );
    }

    /// 回环判断要覆盖 IPv4 与 localhost。
    #[test]
    fn detects_loopback_listen() {
        let mut config = Config {
            listen: "127.0.0.1:8317".into(),
            data_dir: PathBuf::from("."),
            api_key: String::new(),
            admin_key: String::new(),
            ..Config::default()
        };
        assert!(config.is_loopback());
        config.listen = "0.0.0.0:8317".into();
        assert!(!config.is_loopback());
        config.listen = "localhost:8317".into();
        assert!(config.is_loopback());
    }

    /// 默认、显式配置及非法边界都通过纯函数验证，不修改测试进程环境变量。
    #[test]
    fn request_body_limit_validates_mib_and_overflow() {
        assert_eq!(parse_request_body_limit(None).unwrap(), 64 * 1024 * 1024);
        assert_eq!(
            parse_request_body_limit(Some("128")).unwrap(),
            128 * 1024 * 1024
        );
        for value in ["", "0", "-1", "1.5", "unlimited", &usize::MAX.to_string()] {
            assert!(parse_request_body_limit(Some(value)).is_err());
        }
    }
}
