//! 独立临时 Chrome 登录窗口；只读取本窗口 Cookie，不接触用户已有浏览器配置。

use crate::core::AppError;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tempfile::TempDir;
use tokio::{net::TcpStream, process::Child};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

/// 登录窗口和调试通道由会话独占，正常结束会关闭浏览器并删除临时目录。
pub struct Browser {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    child: Child,
    _profile: TempDir,
    sequence: u64,
}

/// 仅从标准安装位置或服务进程配置定位 Chrome，不接受网页指定可执行文件。
fn executable() -> Result<std::path::PathBuf, AppError> {
    if let Some(path) = std::env::var_os("FMP_LOGIN_BROWSER") {
        let path = std::path::PathBuf::from(path);
        if path.is_absolute() && path.is_file() {
            return Ok(path);
        }
        return Err(AppError::bad_request(
            "FMP_LOGIN_BROWSER 必须是 Chrome 的完整可执行文件路径",
        ));
    }
    let candidates = if cfg!(target_os = "macos") {
        vec![
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(),
            "/Applications/Chromium.app/Contents/MacOS/Chromium".into(),
        ]
    } else if cfg!(windows) {
        ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"]
            .iter()
            .filter_map(std::env::var_os)
            .map(|root| std::path::PathBuf::from(root).join("Google/Chrome/Application/chrome.exe"))
            .collect()
    } else {
        [
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
        ]
        .iter()
        .map(std::path::PathBuf::from)
        .collect()
    };
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| AppError::bad_request("未找到 Chrome；请安装 Chrome 后重新登录"))
}

impl Browser {
    /// 在服务所在电脑打开官方页面，临时 profile 和随机调试端口隔离每次登录。
    pub async fn launch(data_dir: &Path, url: &str) -> Result<Self, AppError> {
        Self::launch_with(&executable()?, data_dir, url, false).await
    }

    /// 使用相同启动流程支持无界面离线验收；网页请求不能选择此选项。
    async fn launch_with(
        executable: &Path,
        data_dir: &Path,
        url: &str,
        headless: bool,
    ) -> Result<Self, AppError> {
        tokio::fs::create_dir_all(data_dir)
            .await
            .map_err(|_| AppError::bad_request("无法创建登录临时目录"))?;
        let profile = tempfile::Builder::new()
            .prefix("monkeycode-login-")
            .tempdir_in(data_dir)
            .map_err(|_| AppError::bad_request("无法创建登录临时目录"))?;
        let mut command = tokio::process::Command::new(executable);
        command
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .args([
                "--remote-debugging-address=127.0.0.1",
                "--remote-debugging-port=0",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-sync",
                "--disable-background-networking",
                "--disable-component-update",
            ])
            // 普通浏览器标签页保留地址栏；独立 profile 仍需要单独的浏览器窗口。
            .arg("--new-window")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if headless {
            command.arg("--headless=new");
        }
        let mut child = command
            .spawn()
            .map_err(|_| AppError::bad_request("无法打开 Chrome 登录窗口"))?;
        let connected = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if child
                    .try_wait()
                    .map_err(|_| AppError::bad_request("无法检查登录窗口"))?
                    .is_some()
                {
                    return Err(AppError::bad_request(
                        "登录窗口已关闭或启动失败，请重新开始",
                    ));
                }
                if let Ok(port_file) =
                    tokio::fs::read_to_string(profile.path().join("DevToolsActivePort")).await
                {
                    let mut lines = port_file.lines();
                    let port = lines
                        .next()
                        .and_then(|line| line.parse::<u16>().ok())
                        .filter(|port| *port > 0);
                    let endpoint = lines.next().filter(|line| {
                        line.starts_with("/devtools/browser/")
                            && line
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || b"/-".contains(&byte))
                    });
                    if let (Some(port), Some(endpoint)) = (port, endpoint) {
                        let (socket, _) = tokio_tungstenite::connect_async(format!(
                            "ws://127.0.0.1:{port}{endpoint}"
                        ))
                        .await
                        .map_err(|_| AppError::bad_request("无法连接登录窗口"))?;
                        return Ok(socket);
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        let socket = match connected {
            Ok(Ok(socket)) => socket,
            outcome => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(outcome
                    .err()
                    .map(|_| AppError::bad_request("登录窗口启动超时"))
                    .unwrap_or_else(|| {
                        AppError::bad_request("登录窗口已关闭或无法连接，请重新开始")
                    }));
            }
        };
        Ok(Self {
            socket,
            child,
            _profile: profile,
            sequence: 0,
        })
    }

    /// 按消息 ID 读取本机 CDP 应答，丢弃事件且不将任何协议内容写日志。
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, AppError> {
        self.sequence += 1;
        let id = self.sequence;
        tokio::time::timeout(Duration::from_secs(5), async {
            self.socket
                .send(Message::Text(
                    json!({"id":id,"method":method,"params":params})
                        .to_string()
                        .into(),
                ))
                .await
                .map_err(|_| AppError::bad_request("登录窗口已关闭，请重新开始"))?;
            while let Some(message) = self.socket.next().await {
                let message = message.map_err(|_| AppError::bad_request("登录窗口连接已断开"))?;
                let Message::Text(text) = message else {
                    continue;
                };
                let value: Value = serde_json::from_str(&text)
                    .map_err(|_| AppError::bad_request("登录窗口返回无效应答"))?;
                if value["id"].as_u64() == Some(id) {
                    if value.get("error").is_some() {
                        return Err(AppError::bad_request("读取登录窗口失败，请重新开始"));
                    }
                    return Ok(value["result"].clone());
                }
            }
            Err(AppError::bad_request("登录窗口已关闭，请重新开始"))
        })
        .await
        .map_err(|_| AppError::bad_request("登录窗口响应超时"))?
    }

    /// 将 CDP Cookie 规范化为官方客户端格式；只保留当前账号域且拒绝分区 Cookie。
    pub async fn cookies(&mut self, server: &str) -> Result<Value, AppError> {
        let response = self.call("Storage.getCookies", json!({})).await?;
        let host = reqwest::Url::parse(server)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .ok_or_else(|| AppError::bad_request("MonkeyCode 账号域无效"))?;
        let cookies = response["cookies"]
            .as_array()
            .ok_or_else(|| AppError::bad_request("登录窗口未返回 Cookie 数组"))?;
        Ok(Value::Array(
            cookies
                .iter()
                .filter_map(|cookie| {
                    let domain = cookie["domain"].as_str()?;
                    let stripped = domain.trim_start_matches('.');
                    if !(host == stripped
                        || domain.starts_with('.') && host.ends_with(&format!(".{stripped}")))
                        || cookie.get("partitionKey").is_some()
                    {
                        return None;
                    }
                    let mut value = cookie.clone();
                    value["host_only"] = json!(!domain.starts_with('.'));
                    if let Some(expires) =
                        cookie["expires"].as_f64().filter(|seconds| *seconds > 0.0)
                    {
                        let expires = time::OffsetDateTime::from_unix_timestamp(expires as i64)
                            .ok()?
                            .format(&time::format_description::well_known::Rfc3339)
                            .ok()?;
                        value["expires"] = json!(expires);
                    } else {
                        value.as_object_mut()?.remove("expires");
                    }
                    Some(value)
                })
                .collect(),
        ))
    }

    /// 先正常关闭本窗口，再等待进程退出；兜底只终止本会话启动的进程。
    pub async fn close(mut self) {
        let _ = self.call("Browser.close", json!({})).await;
        if tokio::time::timeout(Duration::from_secs(3), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 使用真实 Chrome 和本机 HTTP 验证 HttpOnly Cookie 隔离、域过滤和窗口清理。
    #[tokio::test]
    #[ignore = "requires a locally installed Chrome"]
    async fn isolated_browser_reads_only_its_own_http_only_cookie() {
        let root = tempfile::Builder::new()
            .prefix(".mc-browser-test-")
            .tempdir_in(".")
            .unwrap();
        let app = axum::Router::new().route(
            "/login",
            axum::routing::get(|| async {
                (
                    [(
                        "set-cookie",
                        "mc_session=offline; HttpOnly; Path=/; SameSite=Lax",
                    )],
                    "Offline login",
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let job = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut first = Browser::launch_with(
            &executable().unwrap(),
            root.path(),
            &format!("{server}/login"),
            true,
        )
        .await
        .unwrap();
        let profile = first._profile.path().to_path_buf();
        let cookies = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let cookies = first.cookies(&server).await.unwrap();
                if !cookies.as_array().unwrap().is_empty() {
                    break cookies;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            super::super::cookie_header_for(&cookies, &server, "/api/v1/users/status").unwrap(),
            "mc_session=offline"
        );
        assert!(
            first
                .cookies("https://monkeycode-ai.net")
                .await
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
        let mut second =
            Browser::launch_with(&executable().unwrap(), root.path(), "about:blank", true)
                .await
                .unwrap();
        assert!(
            second
                .cookies(&server)
                .await
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
        second.close().await;
        first.close().await;
        assert!(!profile.exists());
        job.abort();
    }
}
