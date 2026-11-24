//! 官方 Cap.js 计算式验证码；总预算和取消令牌约束后台计算。

use crate::core::AppError;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

const MAX_HASHES: u64 = 5_000_000;
const COMPUTE_BUDGET: Duration = Duration::from_secs(20);

/// 官方返回的子质询数、盐长度和十六进制前缀长度。
#[derive(Clone, Copy, Deserialize)]
pub(super) struct Challenge {
    pub c: usize,
    pub s: usize,
    pub d: usize,
}

/// 质询端点返回裸结构，expires 使用 Unix 毫秒。
#[derive(Deserialize)]
pub(super) struct Ticket {
    pub token: String,
    pub challenge: Challenge,
    pub expires: u64,
}

/// 计算前校验外部输入，截止时间同时受服务端有效期和本地预算约束。
pub(super) fn deadline(ticket: &Ticket) -> Result<Instant, AppError> {
    if !valid_token(&ticket.token)
        || !(1..=128).contains(&ticket.challenge.c)
        || !(1..=1024).contains(&ticket.challenge.s)
        || !(1..=16).contains(&ticket.challenge.d)
    {
        return Err(AppError::bad_request("MonkeyCode 验证码质询参数无效"));
    }
    Ok(Instant::now() + remaining(ticket.expires)?.min(COMPUTE_BUDGET))
}

/// 令牌仅参与本地计算和同源验证，限制长度以防异常响应占用内存。
pub(super) fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= 4096
}

/// 已过期票据直接拒绝；不把过期时间转换成新的有效期。
pub(super) fn remaining(expires: u64) -> Result<Duration, AppError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let milliseconds = u128::from(expires).saturating_sub(now);
    if milliseconds == 0 {
        return Err(AppError::bad_request("MonkeyCode 验证码已过期，请重试"));
    }
    Ok(Duration::from_millis(
        milliseconds.min(u128::from(u64::MAX)) as u64,
    ))
}

/// FNV-1a 对 UTF-8 字节播种，xorshift32 每轮输出八位小写十六进制。
fn prng(seed: &str, length: usize) -> String {
    let mut state = 0x811c9dc5_u32;
    for byte in seed.bytes() {
        state = (state ^ u32::from(byte)).wrapping_mul(0x01000193);
    }
    let mut output = String::with_capacity(length + 7);
    while output.len() < length {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        output.push_str(&format!("{state:08x}"));
    }
    output.truncate(length);
    output
}

/// 逐半字节比较摘要前缀，避免每个 nonce 分配十六进制摘要字符串。
fn matches(digest: &[u8], target: &[u8]) -> bool {
    target.iter().enumerate().all(|(index, expected)| {
        let byte = digest[index / 2];
        let nibble = if index % 2 == 0 { byte >> 4 } else { byte & 15 };
        nibble == *expected
    })
}

/// 求解整组质询；预算按整组累计，异步任务中断后后台线程也会退出。
pub(super) fn solve(
    ticket: Ticket,
    deadline: Instant,
    cancel: CancellationToken,
) -> Result<Vec<u64>, AppError> {
    solve_with_limit(ticket, deadline, cancel, MAX_HASHES)
}

/// 同时检查时间、取消和哈希次数，测试可缩小预算验证失败边界。
fn solve_with_limit(
    ticket: Ticket,
    deadline: Instant,
    cancel: CancellationToken,
    limit: u64,
) -> Result<Vec<u64>, AppError> {
    // 即使直接调用求解器，也不能跳过参数与服务端有效期校验。
    let deadline = deadline.min(self::deadline(&ticket)?);
    let mut hashes = 0;
    let mut solutions = Vec::with_capacity(ticket.challenge.c);
    for index in 1..=ticket.challenge.c {
        let salt = prng(&format!("{}{index}", ticket.token), ticket.challenge.s);
        let target: Vec<_> = prng(&format!("{}{index}d", ticket.token), ticket.challenge.d)
            .bytes()
            .map(|byte| {
                if byte <= b'9' {
                    byte - b'0'
                } else {
                    byte - b'a' + 10
                }
            })
            .collect();
        let mut salted = Sha256::new();
        salted.update(salt.as_bytes());
        let mut nonce = 0_u64;
        loop {
            if hashes >= limit
                || hashes % 256 == 0 && (cancel.is_cancelled() || Instant::now() >= deadline)
            {
                return Err(AppError::bad_request(
                    "MonkeyCode 验证码计算已取消、过期或超过预算，请重试",
                ));
            }
            hashes += 1;
            let mut hasher = salted.clone();
            hasher.update(nonce.to_string().as_bytes());
            if matches(&hasher.finalize(), &target) {
                solutions.push(nonce);
                break;
            }
            nonce += 1;
        }
    }
    if cancel.is_cancelled() || Instant::now() >= deadline {
        return Err(AppError::bad_request("MonkeyCode 验证码计算已取消或过期"));
    }
    Ok(solutions)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立 Python hashlib 参考向量，固定盐、目标和最小 nonce。
    #[test]
    fn python_reference_vectors_match() {
        assert_eq!(prng("abc", 20), "0bb9adb8ffd8e55f8d1d");
        assert_eq!(prng("fixture-token1", 16), "3942719099790f28");
        let ticket = Ticket {
            token: "fixture-token".into(),
            challenge: Challenge { c: 3, s: 16, d: 2 },
            expires: u64::MAX,
        };
        let expires = deadline(&ticket).unwrap();
        assert_eq!(
            solve(ticket, expires, CancellationToken::new()).unwrap(),
            [121, 481, 362]
        );
    }

    /// 超大参数、缺失令牌和过期票据都不能进入计算。
    #[test]
    fn invalid_parameters_and_expired_tickets_are_rejected() {
        for (c, s, d) in [(0, 32, 3), (129, 32, 3), (50, 1025, 3), (50, 32, 17)] {
            assert!(
                deadline(&Ticket {
                    token: "fixture-token".into(),
                    challenge: Challenge { c, s, d },
                    expires: u64::MAX,
                })
                .is_err()
            );
        }
        assert!(!valid_token(""));
        assert!(!valid_token(&"x".repeat(4097)));
        assert!(remaining(1).is_err());
    }

    /// 整组计算预算、取消和单调时钟截止均能终止求解。
    #[test]
    fn total_budget_cancellation_and_deadline_stop_computation() {
        for mode in 0..3 {
            let ticket = Ticket {
                token: "fixture-token".into(),
                challenge: Challenge { c: 3, s: 16, d: 2 },
                expires: u64::MAX,
            };
            let cancel = CancellationToken::new();
            if mode == 1 {
                cancel.cancel();
            }
            let until = if mode == 2 {
                Instant::now()
            } else {
                deadline(&ticket).unwrap()
            };
            let limit = if mode == 0 { 500 } else { MAX_HASHES };
            assert!(solve_with_limit(ticket, until, cancel, limit).is_err());
        }
    }
}
