//! MonkeyCode 官方网页登录及限定域、路径的登录态筛选。

pub mod browser;
pub mod login;

use crate::core::AppError;
use serde_json::Value;

/// 按实际账号接口匹配 Cookie，避免将其他路径的登录态发送到密钥接口。
fn cookie_header_for(
    cookies: &Value,
    server: &str,
    request_path: &str,
) -> Result<String, AppError> {
    let cookies = cookies
        .as_array()
        .ok_or_else(|| AppError::bad_request("登录窗口未返回合法的 Cookie 数组"))?;
    let url = reqwest::Url::parse(server)
        .map_err(|_| AppError::bad_request("MonkeyCode 账号地址无效"))?;
    let host = url.host_str().unwrap_or_default();
    let now = time::OffsetDateTime::now_utc();
    let mut pairs = Vec::new();
    for cookie in cookies {
        let Some(domain) = cookie["domain"].as_str() else {
            continue;
        };
        let domain = domain.trim_start_matches('.');
        if domain.is_empty()
            || !domain.contains('.') && !matches!(domain, "localhost" | "127.0.0.1" | "::1")
        {
            continue;
        }
        let matches =
            host == domain || cookie["host_only"] != true && host.ends_with(&format!(".{domain}"));
        let path = cookie["path"].as_str().unwrap_or("/");
        if !matches
            || !(request_path == path
                || request_path.starts_with(&format!("{}/", path.trim_end_matches('/'))))
            || cookie["secure"] == true && url.scheme() != "https"
        {
            continue;
        }
        if let Some(expires) = cookie["expires"].as_str() {
            let Ok(expires) = time::OffsetDateTime::parse(
                expires,
                &time::format_description::well_known::Rfc3339,
            ) else {
                continue;
            };
            if expires <= now {
                continue;
            }
        }
        let Some(name) = cookie["name"].as_str().filter(|name| !name.is_empty()) else {
            continue;
        };
        let Some(value) = cookie["value"].as_str() else {
            continue;
        };
        if name.contains([';', '=', '\r', '\n']) || value.contains([';', '\r', '\n']) {
            return Err(AppError::bad_request("Cookie 包含无效字符"));
        }
        pairs.push(format!("{name}={value}"));
    }
    if pairs.is_empty() {
        return Err(AppError::bad_request(
            "登录窗口没有该区域的有效 Cookie，请重新登录",
        ));
    }
    Ok(pairs.join("; "))
}
