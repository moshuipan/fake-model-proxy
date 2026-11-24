//! 只用带实例随机密钥的指纹保存历史连续性；正文和密钥不落盘。

use super::{ClientKind, SessionConfidence, SessionEvidence, SessionLink, SessionSource};
use crate::core::{ChatRequest, ContentPart, Message, Role, ToolCall};
use rand_core::{OsRng, RngCore};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

type Fingerprint = [u8; 32];
const TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_HISTORY_MESSAGES: usize = 4096;

/// 一个请求的历史前缀指纹，不保留消息内容。
pub struct HistorySnapshot {
    prefixes: Vec<Fingerprint>,
    continuations: Vec<bool>,
    anchors: Vec<(Fingerprint, usize)>,
    normalized_tool_ids: usize,
}

struct Frame {
    digest: Fingerprint,
    length: usize,
    link: SessionLink,
    request_id: String,
    completed: Instant,
    anchors: Vec<Fingerprint>,
}

/// 完成输出的消息摘要与有界工具锚点，均不保存正文。
pub struct ReplyEvidence {
    digest: Fingerprint,
    anchors: Vec<Fingerprint>,
}

/// 已交付响应的有界索引，与后台配置热加载独立。
pub struct HistoryIndex {
    key: [u8; 32],
    capacity: usize,
    frames: Mutex<VecDeque<Frame>>,
}

/// 单条助手输出的流式文本指纹，不额外累计完整输出。
pub struct ReplyFingerprint {
    text: Sha256,
    client: ClientKind,
}

/// 长度前缀避免不同字段串接得到同一输入。
fn field(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value);
}

/// 为不同指纹用途创建隔离的摘要状态。
fn hasher(key: &[u8; 32], domain: &[u8]) -> Sha256 {
    let mut hash = Sha256::new();
    field(&mut hash, key);
    field(&mut hash, domain);
    hash
}

/// 内容与工具结构均参与指纹；JSON 参数忽略无意义的排版差异。
fn message_digest(
    key: &[u8; 32],
    role: Role,
    text: Fingerprint,
    images: &[(usize, &str, Option<&str>)],
    calls: &[ToolCall],
    tool_call_id: Option<&str>,
    name: Option<&str>,
) -> Fingerprint {
    let mut hash = hasher(key, b"message");
    field(
        &mut hash,
        match role {
            Role::User => b"user",
            Role::Assistant => b"assistant",
            Role::Tool => b"tool",
            _ => b"system",
        },
    );
    field(&mut hash, &text);
    hash.update((images.len() as u64).to_le_bytes());
    for (offset, url, detail) in images {
        hash.update((*offset as u64).to_le_bytes());
        field(&mut hash, url.as_bytes());
        field(&mut hash, detail.unwrap_or_default().as_bytes());
    }
    hash.update((calls.len() as u64).to_le_bytes());
    for call in calls {
        field(&mut hash, call.id.as_bytes());
        field(&mut hash, call.name.as_bytes());
        let canonical = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .and_then(|value| serde_json::to_vec(&value).ok())
            .unwrap_or_else(|| call.arguments.as_bytes().to_vec());
        field(&mut hash, &canonical);
    }
    field(&mut hash, tool_call_id.unwrap_or_default().as_bytes());
    field(&mut hash, name.unwrap_or_default().as_bytes());
    hash.finalize().into()
}

/// 相邻文本片段归一；图片位置仍参与指纹，防止内容顺序不同却匹配。
fn input_digest(key: &[u8; 32], message: &Message) -> Fingerprint {
    let mut text = hasher(key, b"text");
    let mut offset = 0;
    let mut images = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text: part } => {
                text.update(part.as_bytes());
                offset += part.len();
            }
            ContentPart::ImageUrl { url, detail } => {
                images.push((offset, url.as_str(), detail.as_deref()));
            }
        }
    }
    message_digest(
        key,
        message.role,
        text.finalize().into(),
        &images,
        &message.tool_calls,
        message.tool_call_id.as_deref(),
        message.name.as_deref(),
    )
}

/// 只对已选择 TRAE 插件且结构完整的包装提取实际输入，普通文本保留原样。
fn trae_input(text: &str) -> &str {
    let Some((before, tail)) = text.split_once("<user_input>") else {
        return text;
    };
    let Some((input, after)) = tail.split_once("</user_input>") else {
        return text;
    };
    if input.contains("<user_input>")
        || after.contains("<user_input>")
        || !(before.trim().is_empty()
            || before.contains("<system-reminder>")
            || before.contains("<environment_context>"))
    {
        return text;
    }
    input.trim()
}

/// 协议拆出的连续助手 item 归为同一逻辑输出，工具结构及顺序保留。
fn logical_messages(request: &ChatRequest, client: ClientKind) -> Option<Vec<Message>> {
    if request.messages.len() > MAX_HISTORY_MESSAGES {
        return None;
    }
    let mut messages: Vec<Message> = Vec::new();
    for original in &request.messages {
        if matches!(original.role, Role::System | Role::Developer) {
            continue;
        }
        if original.tool_calls.len() > 64 {
            return None;
        }
        let mut message = original.clone();
        if client == ClientKind::Trae && message.role == Role::User && !message.has_image() {
            let text = message.text_content();
            message.content = vec![ContentPart::text(trae_input(&text))];
        }
        if message.role == Role::Assistant
            && let Some(previous) = messages.last_mut()
            && previous.role == Role::Assistant
            && previous.name == message.name
            && previous.tool_call_id.is_none()
            && message.tool_call_id.is_none()
        {
            if previous.tool_calls.len() + message.tool_calls.len() > 64 {
                return None;
            }
            previous.content.extend(message.content);
            previous.tool_calls.extend(message.tool_calls);
        } else {
            messages.push(message);
        }
    }
    if client == ClientKind::Trae {
        normalize_trae_tool_ids(&mut messages)?;
    }
    Some(messages)
}

/// TRAE 会重写工具 ID；摘要保留调用语义和结果配对，原请求不被改写。
fn normalize_trae_tool_ids(messages: &mut [Message]) -> Option<()> {
    let mut calls = HashMap::new();
    for message in messages {
        for call in &mut message.tool_calls {
            let id = call.id.clone();
            call.id = semantic_tool_id(call);
            calls.insert(id, call.id.clone());
        }
        if message.role == Role::Tool {
            let id = message.tool_call_id.as_ref()?;
            message.tool_call_id = Some(calls.get(id)?.clone());
        }
    }
    Some(())
}

/// 用可逆分隔的调用语义替代随机标识，仅在计算摘要的临时消息中使用。
fn semantic_tool_id(call: &ToolCall) -> String {
    let canonical = serde_json::from_str::<serde_json::Value>(&call.arguments)
        .ok()
        .and_then(|value| serde_json::to_string(&value).ok())
        .unwrap_or_else(|| call.arguments.clone());
    serde_json::to_string(&(&call.name, canonical)).expect("string tuple is JSON")
}

/// 裁剪恢复要求完整工具结构参与匹配，单个通用 call_1 字符串不能充当身份。
fn tool_anchor(key: &[u8; 32], call: &ToolCall) -> Fingerprint {
    message_digest(
        key,
        Role::Assistant,
        hasher(key, b"text").finalize().into(),
        &[],
        std::slice::from_ref(call),
        None,
        None,
    )
}

impl HistoryIndex {
    /// 实例随机密钥保证重启后指纹不可复用，索引不包含可离线猜测的裸摘要。
    pub fn new(capacity: usize) -> Self {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        Self {
            key,
            capacity: capacity.max(1),
            frames: Mutex::new(VecDeque::new()),
        }
    }

    /// 链式摘要允许只保存上一历史摘要与消息长度。
    fn extend(&self, previous: &Fingerprint, message: &Fingerprint) -> Fingerprint {
        let mut hash = hasher(&self.key, b"history-step");
        field(&mut hash, previous);
        field(&mut hash, message);
        hash.finalize().into()
    }

    /// 对完整归一历史计算前缀；超出有界索引能力时不推断。
    pub fn snapshot(&self, request: &ChatRequest) -> Option<HistorySnapshot> {
        self.snapshot_for(request, ClientKind::Generic)
    }

    /// 专用客户端规范化仅作用于摘要，不改写发送给上游的请求。
    pub fn snapshot_for(
        &self,
        request: &ChatRequest,
        client: ClientKind,
    ) -> Option<HistorySnapshot> {
        let messages = logical_messages(request, client)?;
        let mut previous: Fingerprint = hasher(&self.key, b"history-root").finalize().into();
        let mut prefixes = Vec::new();
        let mut continuations = Vec::new();
        let mut has_user = false;
        let mut anchors = Vec::new();
        let normalized_tool_ids = if client == ClientKind::Trae {
            messages
                .iter()
                .map(|message| message.tool_calls.len())
                .sum()
        } else {
            0
        };
        for (i, message) in messages.iter().enumerate() {
            if client == ClientKind::Trae {
                if !message.tool_calls.is_empty()
                    && anchors.len() < 64
                    && message.tool_calls.iter().all(|call| {
                        messages[i + 1..].iter().any(|result| {
                            result.role == Role::Tool
                                && result.tool_call_id.as_deref() == Some(&call.id)
                        })
                    })
                {
                    anchors.push((input_digest(&self.key, message), message.tool_calls.len()));
                }
                continue;
            }
            for call in &message.tool_calls {
                if anchors.len() < 64
                    && messages[i + 1..].iter().any(|result| {
                        result.role == Role::Tool
                            && result.tool_call_id.as_deref() == Some(&call.id)
                    })
                {
                    anchors.push((tool_anchor(&self.key, call), 1));
                }
            }
        }
        for message in &messages {
            if prefixes.len() == MAX_HISTORY_MESSAGES {
                return None;
            }
            has_user |= message.role == Role::User;
            previous = self.extend(&previous, &input_digest(&self.key, message));
            prefixes.push(previous);
            continuations.push(matches!(message.role, Role::User | Role::Tool));
        }
        (has_user || !anchors.is_empty()).then_some(HistorySnapshot {
            prefixes,
            continuations,
            anchors,
            normalized_tool_ids,
        })
    }

    /// 只匹配完整已交付历史，并选择最长匹配；多组同样匹配时显式拆组。
    pub fn resolve(&self, client: ClientKind, snapshot: &HistorySnapshot) -> SessionLink {
        let mut source = SessionSource::HistoryNew;
        let mut evidence = SessionEvidence {
            logical_messages: snapshot.prefixes.len(),
            matched_messages: 0,
            matched_tool_calls: 0,
            candidate_groups: 0,
            normalized_tool_ids: snapshot.normalized_tool_ids,
        };
        if let Ok(mut frames) = self.frames.lock() {
            frames.retain(|frame| frame.completed.elapsed() <= TTL);
            let candidates: Vec<_> = frames
                .iter()
                .filter(|frame| {
                    frame.link.client == Some(client)
                        && frame.length < snapshot.prefixes.len()
                        && snapshot.continuations.get(frame.length) == Some(&true)
                        && snapshot.prefixes.get(frame.length - 1) == Some(&frame.digest)
                })
                .collect();
            if let Some(length) = candidates.iter().map(|frame| frame.length).max() {
                let candidates: Vec<_> = candidates
                    .into_iter()
                    .filter(|frame| frame.length == length)
                    .collect();
                let groups: HashSet<_> = candidates.iter().map(|frame| &frame.link.id).collect();
                evidence.matched_messages = length;
                evidence.candidate_groups = groups.len();
                if groups.len() == 1 {
                    let frame = candidates.last().expect("nonempty matching group");
                    let mut link = frame.link.clone();
                    link.source = SessionSource::HistoryContinuation;
                    link.previous_request_id = Some(frame.request_id.clone());
                    link.evidence = Some(evidence);
                    return link;
                }
                source = SessionSource::HistoryAmbiguous;
            } else if !snapshot.anchors.is_empty() {
                let candidates: Vec<_> = frames
                    .iter()
                    .filter(|frame| {
                        frame.link.client == Some(client)
                            && frame.anchors.iter().any(|anchor| {
                                snapshot.anchors.iter().any(|(input, _)| input == anchor)
                            })
                    })
                    .collect();
                let groups: HashSet<_> = candidates.iter().map(|frame| &frame.link.id).collect();
                evidence.candidate_groups = groups.len();
                if groups.len() == 1 {
                    let frame = candidates.last().expect("unique anchor group");
                    evidence.matched_messages = 1;
                    evidence.matched_tool_calls = snapshot
                        .anchors
                        .iter()
                        .filter(|(anchor, _)| frame.anchors.contains(anchor))
                        .map(|(_, calls)| calls)
                        .sum();
                    let mut link = frame.link.clone();
                    link.source = SessionSource::HistoryToolAnchor;
                    link.previous_request_id = Some(frame.request_id.clone());
                    link.evidence = Some(evidence);
                    return link;
                }
                if groups.len() > 1 {
                    source = SessionSource::HistoryAmbiguous;
                }
            }
        }
        SessionLink {
            id: format!(
                "{}:inferred_{}",
                client.as_str(),
                uuid::Uuid::new_v4().simple()
            ),
            client: Some(client),
            source,
            confidence: SessionConfidence::Inferred,
            client_session_id: None,
            previous_request_id: None,
            evidence: Some(evidence),
        }
    }

    /// 创建按增量更新的输出指纹，正文不会增加索引内存。
    pub fn reply(&self) -> ReplyFingerprint {
        self.reply_for(ClientKind::Generic)
    }

    /// 输出与输入采用同一客户端规则，避免把 TRAE 重写 ID 视为另一段历史。
    pub fn reply_for(&self, client: ClientKind) -> ReplyFingerprint {
        ReplyFingerprint {
            text: hasher(&self.key, b"text"),
            client,
        }
    }

    /// 工具调用取编码器共用的完整结果，保证输入与实际输出语义一致。
    pub fn reply_digest(&self, reply: &ReplyFingerprint, calls: &[ToolCall]) -> ReplyEvidence {
        let mut normalized_calls = calls.to_vec();
        if reply.client == ClientKind::Trae {
            for call in &mut normalized_calls {
                call.id = semantic_tool_id(call);
            }
        }
        let digest = message_digest(
            &self.key,
            Role::Assistant,
            reply.text.clone().finalize().into(),
            &[],
            &normalized_calls,
            None,
            None,
        );
        ReplyEvidence {
            digest,
            anchors: if reply.client == ClientKind::Trae {
                (!calls.is_empty()).then_some(digest).into_iter().collect()
            } else {
                calls
                    .iter()
                    .take(64)
                    .map(|call| tool_anchor(&self.key, call))
                    .collect()
            },
        }
    }

    /// 仅由完整交付终态发布证据；失败、取消与显式 ID 均不参与推断索引。
    pub fn remember(
        &self,
        snapshot: HistorySnapshot,
        reply: ReplyEvidence,
        link: SessionLink,
        request_id: String,
    ) {
        if link.confidence != SessionConfidence::Inferred {
            return;
        }
        let Some(previous) = snapshot.prefixes.last() else {
            return;
        };
        if let Ok(mut frames) = self.frames.lock() {
            frames.retain(|frame| frame.completed.elapsed() <= TTL);
            while frames.len() >= self.capacity {
                frames.pop_front();
            }
            frames.push_back(Frame {
                digest: self.extend(previous, &reply.digest),
                length: snapshot.prefixes.len() + 1,
                link,
                request_id,
                completed: Instant::now(),
                anchors: reply.anchors,
            });
        }
    }
}

impl ReplyFingerprint {
    /// 分片边界不影响文本指纹，兼容不同上游 SSE 分片方式。
    pub fn text(&mut self, text: &str) {
        self.text.update(text.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Protocol;

    /// 用最小归一请求测试指纹，不经过真实模型或客户端。
    fn request(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            protocol: Protocol::Chat,
            model: "mock".into(),
            messages,
            tools: vec![],
            tool_choice: None,
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: vec![],
            reasoning_effort: None,
            stream: true,
            session: None,
        }
    }

    /// 构造已交付的文本与工具调用，包含可检查的客户端随机 ID。
    fn call(id: &str, value: u32) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: format!("{{\"x\":{value}}}"),
        }
    }

    /// 构造协议拆分后会重新合并的助手输出与匹配的工具结果。
    fn continuation(id: &str, text: &str, value: u32) -> Vec<Message> {
        let mut assistant = Message::text(Role::Assistant, text);
        assistant.tool_calls = vec![call(id, value)];
        let mut result = Message::text(Role::Tool, "result");
        result.tool_call_id = Some(id.into());
        vec![Message::text(Role::User, "task"), assistant, result]
    }

    /// 只通过正常完成入口发布输出证据。
    fn remember(index: &HistoryIndex, client: ClientKind, text: &str, id: &str) -> SessionLink {
        let snapshot = index
            .snapshot_for(&request(vec![Message::text(Role::User, "task")]), client)
            .unwrap();
        let link = index.resolve(client, &snapshot);
        let mut reply = index.reply_for(client);
        reply.text(text);
        index.remember(
            snapshot,
            index.reply_digest(&reply, &[call("upstream", 1)]),
            link.clone(),
            id.into(),
        );
        link
    }

    /// TRAE 的随机 ID 变化仍匹配最近输出，通用客户端保留原有标识语义。
    #[test]
    fn rewritten_trae_ids_match_delivered_output_and_preserve_request() {
        for client in [ClientKind::Trae, ClientKind::Generic] {
            let index = HistoryIndex::new(20);
            let original = remember(&index, client, "reply", "req-a");
            let input = request(continuation("client-rewritten", "reply", 1));
            let snapshot = index.snapshot_for(&input, client).unwrap();
            let link = index.resolve(client, &snapshot);
            assert_eq!(link.id == original.id, client == ClientKind::Trae);
            assert_eq!(input.messages[1].tool_calls[0].id, "client-rewritten");
            assert_eq!(
                input.messages[2].tool_call_id.as_deref(),
                Some("client-rewritten")
            );
            if client == ClientKind::Trae {
                assert_eq!(link.previous_request_id.as_deref(), Some("req-a"));
                assert_eq!(link.evidence.as_ref().unwrap().matched_messages, 2);
                assert_eq!(link.evidence.as_ref().unwrap().normalized_tool_ids, 1);
                let mut reply = index.reply_for(client);
                reply.text("next");
                index.remember(
                    snapshot,
                    index.reply_digest(&reply, &[]),
                    link.clone(),
                    "req-b".into(),
                );
                let mut next = continuation("rewritten-again", "reply", 1);
                next.extend([
                    Message::text(Role::Assistant, "next"),
                    Message::text(Role::User, "continue"),
                ]);
                let next =
                    index.resolve(client, &index.snapshot_for(&request(next), client).unwrap());
                assert_eq!(next.previous_request_id.as_deref(), Some("req-b"));
                assert_eq!(next.id, original.id);
            }
        }
    }

    /// 裁剪恢复要求完整交付文本、工具语义和结果关系，不能仅凭工具名称合并。
    #[test]
    fn trae_anchor_requires_complete_reply_and_unique_group() {
        let index = HistoryIndex::new(20);
        let original = remember(&index, ClientKind::Trae, "reply", "req-a");
        let resolve = |messages| {
            index.resolve(
                ClientKind::Trae,
                &index
                    .snapshot_for(&request(messages), ClientKind::Trae)
                    .unwrap(),
            )
        };
        let full = continuation("new-id", "reply", 1);
        let cropped = full[1..].to_vec();
        let link = resolve(cropped.clone());
        assert_eq!(link.id, original.id);
        assert_eq!(link.source, SessionSource::HistoryToolAnchor);
        assert_eq!(link.evidence.as_ref().unwrap().matched_tool_calls, 1);
        for (text, value) in [("changed", 1), ("reply", 2)] {
            let changed = continuation("new-id", text, value);
            assert_ne!(resolve(changed).id, original.id);
        }
        let other = remember(&index, ClientKind::Trae, "reply", "req-other");
        assert_ne!(original.id, other.id);
        let ambiguous = resolve(cropped);
        assert_eq!(ambiguous.source, SessionSource::HistoryAmbiguous);
        assert_eq!(ambiguous.evidence.unwrap().candidate_groups, 2);
        let ambiguous = resolve(full);
        assert_eq!(ambiguous.source, SessionSource::HistoryAmbiguous);
        assert_eq!(ambiguous.evidence.unwrap().candidate_groups, 2);
    }

    /// 工具结果换绑到另一种调用时，规范化后的完整历史仍须不同。
    #[test]
    fn trae_tool_result_pairing_is_part_of_history() {
        let index = HistoryIndex::new(20);
        let mut input = continuation("one", "reply", 1);
        input[1].tool_calls.push(call("two", 2));
        let mut second_result = Message::text(Role::Tool, "second result");
        second_result.tool_call_id = Some("two".into());
        input.push(second_result);
        let snapshot = index
            .snapshot_for(&request(input.clone()), ClientKind::Trae)
            .unwrap();
        let link = index.resolve(ClientKind::Trae, &snapshot);
        let mut reply = index.reply_for(ClientKind::Trae);
        reply.text("done");
        index.remember(
            snapshot,
            index.reply_digest(&reply, &[]),
            link.clone(),
            "req-a".into(),
        );
        input[2].tool_call_id = Some("two".into());
        input[3].tool_call_id = Some("one".into());
        input.extend([
            Message::text(Role::Assistant, "done"),
            Message::text(Role::User, "next"),
        ]);
        let changed = index.resolve(
            ClientKind::Trae,
            &index
                .snapshot_for(&request(input), ClientKind::Trae)
                .unwrap(),
        );
        assert_ne!(changed.id, link.id);
    }
}
