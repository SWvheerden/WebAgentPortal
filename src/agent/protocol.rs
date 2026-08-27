//! The `stream-json` line protocol spoken over the CLI's stdin/stdout, plus the
//! `control_request` / `control_response` channel layered on top of it.
//!
//! Parsing is deliberately tolerant: every line is kept as raw JSON and an
//! unrecognised `type` becomes [`CliEvent::Unknown`] rather than an error, so a
//! protocol change surfaces in the UI instead of killing the reader.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// The `kind` column of the `events` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    User,
    Assistant,
    ToolUse,
    ToolResult,
    PermissionRequest,
    PermissionDecision,
    System,
    Result,
    Stderr,
    Error,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::User => "user",
            EventKind::Assistant => "assistant",
            EventKind::ToolUse => "tool_use",
            EventKind::ToolResult => "tool_result",
            EventKind::PermissionRequest => "permission_request",
            EventKind::PermissionDecision => "permission_decision",
            EventKind::System => "system",
            EventKind::Result => "result",
            EventKind::Stderr => "stderr",
            EventKind::Error => "error",
        }
    }
}

impl std::str::FromStr for EventKind {
    type Err = UnknownEventKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user" => Ok(EventKind::User),
            "assistant" => Ok(EventKind::Assistant),
            "tool_use" => Ok(EventKind::ToolUse),
            "tool_result" => Ok(EventKind::ToolResult),
            "permission_request" => Ok(EventKind::PermissionRequest),
            "permission_decision" => Ok(EventKind::PermissionDecision),
            "system" => Ok(EventKind::System),
            "result" => Ok(EventKind::Result),
            "stderr" => Ok(EventKind::Stderr),
            "error" => Ok(EventKind::Error),
            other => Err(UnknownEventKind(other.to_string())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown event kind: {0}")]
pub struct UnknownEventKind(pub String);

/// A `system` line. `subtype` distinguishes `init` from everything else
/// (`permission_denied`, compaction notices, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemLine {
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub slash_commands: Vec<Value>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// An `assistant` or `user` line. The nested `message` follows the Messages API
/// shape, so `content` is either a bare string or an array of blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageLine {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
    /// Set when the CLI synthesises an `assistant` line to carry an API
    /// failure — a rate limit, say. The model did not say this, and it must not
    /// be shown as though it had.
    #[serde(default)]
    pub is_api_error_message: bool,
    /// The failure's category when `is_api_error_message` is set: `rate_limit`
    /// and so on.
    #[serde(default)]
    pub error: Option<String>,
    pub message: Value,
}

impl MessageLine {
    /// The message's content blocks, normalised to an array. A bare string
    /// becomes a single `text` block.
    pub fn blocks(&self) -> Vec<Value> {
        match self.message.get("content") {
            Some(Value::Array(items)) => items.clone(),
            Some(Value::String(text)) => vec![json!({"type": "text", "text": text})],
            _ => Vec::new(),
        }
    }

    /// Concatenated text of every `text` block.
    pub fn text(&self) -> String {
        self.blocks()
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("")
    }
}

/// A `result` line: one per turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultLine {
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub num_turns: Option<u64>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
    #[serde(default)]
    pub result: Option<String>,
    /// The HTTP status of the API call that ended the turn, when one did.
    /// 429 is the rate limit.
    #[serde(default)]
    pub api_error_status: Option<u64>,
}

impl ResultLine {
    /// How this turn failed, in words, or `None` if it did not.
    ///
    /// `subtype` is no help: a turn killed by a 429 still reports
    /// `subtype: "success"`. `is_error` is the flag that means it, and
    /// `result` carries the CLI's own wording, which is the part worth
    /// repeating — it names the limit and when it resets.
    pub fn failure(&self) -> Option<String> {
        if !self.is_error {
            return None;
        }
        let status = self
            .api_error_status
            .map(|s| format!(" (HTTP {s})"))
            .unwrap_or_default();
        let detail = self
            .result
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| format!(": {t}"))
            .unwrap_or_default();
        Some(format!("The turn ended in an error{status}{detail}"))
    }
}

/// A `control_request` from the CLI to us. The only subtype we act on is
/// `can_use_tool`, but the envelope is kept whole either way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlRequestLine {
    pub request_id: String,
    pub request: Value,
}

impl ControlRequestLine {
    pub fn subtype(&self) -> Option<&str> {
        self.request.get("subtype").and_then(Value::as_str)
    }

    /// Interpret this request as a tool permission prompt.
    pub fn as_permission_request(&self) -> Option<PermissionRequest> {
        if self.subtype() != Some("can_use_tool") {
            return None;
        }
        let get = |k: &str| {
            self.request
                .get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        Some(PermissionRequest {
            request_id: self.request_id.clone(),
            tool_name: get("tool_name").unwrap_or_else(|| "unknown".to_string()),
            display_name: get("display_name"),
            description: get("description"),
            tool_use_id: get("tool_use_id"),
            input: self.request.get("input").cloned().unwrap_or(Value::Null),
            permission_suggestions: self
                .request
                .get("permission_suggestions")
                .cloned()
                .unwrap_or(Value::Null),
        })
    }
}

/// A tool permission prompt awaiting a human decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub request_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub permission_suggestions: Value,
}

/// A `control_response` from the CLI, answering a request we sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponseLine {
    #[serde(default)]
    pub response: Value,
}

impl ControlResponseLine {
    pub fn request_id(&self) -> Option<&str> {
        self.response.get("request_id").and_then(Value::as_str)
    }

    pub fn is_error(&self) -> bool {
        self.response.get("subtype").and_then(Value::as_str) == Some("error")
    }

    /// The inner payload, e.g. `{"still_queued": [...]}` or the command list.
    pub fn payload(&self) -> Option<&Value> {
        self.response.get("response")
    }
}

/// A `rate_limit_event`: the account's usage against its windows. The CLI emits
/// one whenever the numbers change, which in practice is once per API request.
///
/// The envelope is snake_case but `rate_limit_info` is camelCase — that is the
/// CLI's shape, not a typo. Unknown keys are kept in `extra` so a field added
/// upstream reaches the UI without a parser change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitLine {
    pub rate_limit_info: RateLimitInfo,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Usage against the rate-limit windows. Only `status` is guaranteed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitInfo {
    /// `allowed`, `allowed_warning` or `rejected`.
    pub status: String,
    /// Unix **seconds** (not millis) at which the governing window resets.
    #[serde(default)]
    pub resets_at: Option<i64>,
    /// Which window is governing: `five_hour`, `seven_day`, `overage`, …
    #[serde(default)]
    pub rate_limit_type: Option<String>,
    /// Fraction of the governing window used, 0.0–1.0.
    #[serde(default)]
    pub utilization: Option<f64>,
    #[serde(default)]
    pub is_using_overage: Option<bool>,
    /// Per-window usage, keyed `five_hour` / `seven_day` /
    /// `seven_day_overage_included`. Absent on accounts that do not report it.
    #[serde(default)]
    pub unified_windows: BTreeMap<String, RateLimitWindow>,
    /// Everything else the CLI sent: `overageStatus`, `errorCode`, and whatever
    /// is added next.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One window's usage. Both fields are required in the CLI's schema but
/// optional here: a window the CLI reports oddly should cost that one meter,
/// not turn the whole event into an unrecognised line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    /// Fraction used, 0.0–1.0.
    #[serde(default)]
    pub utilization: Option<f64>,
    /// Unix seconds.
    #[serde(default)]
    pub resets_at: Option<i64>,
}

/// A `tool_progress` line. The CLI emits one every 30s for any tool still
/// running (`heartbeat`), and one per retry when a subagent's API call fails.
///
/// The `bash_progress`-derived variant carries incremental output, but the CLI
/// gates that behind `CLAUDE_CODE_REMOTE`/`CLAUDE_CODE_CONTAINER_ID`, so a
/// local child never sends it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolProgressLine {
    /// Synthetic on a heartbeat: `<real tool_use_id>-heartbeat-<n>`. It never
    /// matches a `tool_use` block, so it is not a key to look tools up by.
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    /// The tool the heartbeat is about for a top-level call, and the enclosing
    /// Agent call for one inside a subagent — *not* a plain "is nested" flag.
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
    #[serde(default)]
    pub elapsed_time_seconds: Option<u64>,
    #[serde(default)]
    pub heartbeat: Option<bool>,
    #[serde(default)]
    pub subagent_type: Option<String>,
    /// Present only while a subagent's API call is being retried.
    #[serde(default)]
    pub subagent_retry: Option<SubagentRetry>,
}

/// A subagent whose API call failed and is being retried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentRetry {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub attempt: Option<u32>,
    #[serde(default)]
    pub max_retries: Option<u32>,
    #[serde(default)]
    pub retry_delay_ms: Option<u64>,
    #[serde(default)]
    pub error_status: Option<i64>,
    #[serde(default)]
    pub error_category: Option<String>,
}

impl SubagentRetry {
    /// "subagent Explore retrying (2/3) after HTTP 529" — the operator-facing
    /// line. `subagent_type` comes from the enclosing event.
    pub fn describe(&self, subagent_type: Option<&str>) -> String {
        let who = subagent_type.unwrap_or("subagent");
        let mut text = format!("subagent {who} is retrying");
        if let (Some(n), Some(max)) = (self.attempt, self.max_retries) {
            text.push_str(&format!(" ({n}/{max})"));
        }
        match (self.error_status, self.error_category.as_deref()) {
            (Some(code), _) => text.push_str(&format!(" after HTTP {code}")),
            (None, Some(cat)) => text.push_str(&format!(" after {cat}")),
            (None, None) => {}
        }
        text
    }
}

/// A recognised line from the CLI's stdout.
#[derive(Debug, Clone)]
pub enum CliEvent {
    System(SystemLine),
    Assistant(MessageLine),
    User(MessageLine),
    Result(ResultLine),
    /// Partial-token deltas. Broadcast live, never persisted.
    StreamEvent(Value),
    ControlRequest(ControlRequestLine),
    ControlResponse(ControlResponseLine),
    /// Account usage against the rate-limit windows.
    RateLimit(RateLimitLine),
    /// A tool is still running, or a subagent is retrying.
    ToolProgress(Box<ToolProgressLine>),
    /// A line we could not classify. `reason` explains why; `kind` is the
    /// reported `type` (or `"<invalid json>"`).
    Unknown {
        kind: String,
        reason: String,
    },
}

impl CliEvent {
    pub fn type_name(&self) -> &str {
        match self {
            CliEvent::System(_) => "system",
            CliEvent::Assistant(_) => "assistant",
            CliEvent::User(_) => "user",
            CliEvent::Result(_) => "result",
            CliEvent::StreamEvent(_) => "stream_event",
            CliEvent::ControlRequest(_) => "control_request",
            CliEvent::ControlResponse(_) => "control_response",
            CliEvent::RateLimit(_) => "rate_limit_event",
            CliEvent::ToolProgress(_) => "tool_progress",
            CliEvent::Unknown { kind, .. } => kind,
        }
    }
}

/// A parsed stdout line: the verbatim JSON plus its interpretation.
#[derive(Debug, Clone)]
pub struct ParsedLine {
    pub raw: Value,
    pub event: CliEvent,
}

/// Parse one line of the CLI's stdout.
///
/// This never fails: malformed JSON and unknown `type` values both come back as
/// [`CliEvent::Unknown`], with the raw text preserved so nothing is lost.
pub fn parse_line(line: &str) -> ParsedLine {
    let raw: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(err) => {
            return ParsedLine {
                raw: json!({"type": "error", "text": line, "parse_error": err.to_string()}),
                event: CliEvent::Unknown {
                    kind: "<invalid json>".to_string(),
                    reason: err.to_string(),
                },
            };
        }
    };

    let ty = raw
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // Typed view of the line; a shape mismatch degrades to Unknown, it never panics.
    fn typed<T: for<'de> Deserialize<'de>>(raw: &Value, f: impl FnOnce(T) -> CliEvent) -> CliEvent {
        match serde_json::from_value::<T>(raw.clone()) {
            Ok(v) => f(v),
            Err(err) => CliEvent::Unknown {
                kind: raw
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("<untyped>")
                    .to_string(),
                reason: err.to_string(),
            },
        }
    }

    let event = match ty.as_str() {
        "system" => typed::<SystemLine>(&raw, CliEvent::System),
        "assistant" => typed::<MessageLine>(&raw, CliEvent::Assistant),
        "user" => typed::<MessageLine>(&raw, CliEvent::User),
        "result" => typed::<ResultLine>(&raw, CliEvent::Result),
        "stream_event" => CliEvent::StreamEvent(raw.clone()),
        "control_request" => typed::<ControlRequestLine>(&raw, CliEvent::ControlRequest),
        "control_response" => typed::<ControlResponseLine>(&raw, CliEvent::ControlResponse),
        "rate_limit_event" => typed::<RateLimitLine>(&raw, CliEvent::RateLimit),
        "tool_progress" => typed::<ToolProgressLine>(&raw, |l| CliEvent::ToolProgress(Box::new(l))),
        "" => CliEvent::Unknown {
            kind: "<no type>".to_string(),
            reason: "line has no `type` field".to_string(),
        },
        other => CliEvent::Unknown {
            kind: other.to_string(),
            reason: "unrecognised event type".to_string(),
        },
    };

    ParsedLine { raw, event }
}

/// A `tool_use` block lifted out of an assistant message.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolUse {
    pub id: Option<String>,
    pub name: String,
    pub input: Value,
}

impl ToolUse {
    /// A short "Bash: cargo test" style label for the `Working` sub-status.
    pub fn label(&self) -> String {
        let hint = self
            .input
            .get("command")
            .or_else(|| self.input.get("file_path"))
            .or_else(|| self.input.get("path"))
            .or_else(|| self.input.get("pattern"))
            .or_else(|| self.input.get("description"))
            .and_then(Value::as_str);
        match hint {
            Some(h) => {
                let h = h.trim();
                let short: String = h.chars().take(60).collect();
                if short.len() < h.len() {
                    format!("{}: {short}…", self.name)
                } else {
                    format!("{}: {short}", self.name)
                }
            }
            None => self.name.clone(),
        }
    }
}

/// Extract the `tool_use` blocks from an assistant message.
pub fn tool_uses(msg: &MessageLine) -> Vec<ToolUse> {
    msg.blocks()
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .map(|b| ToolUse {
            id: b.get("id").and_then(Value::as_str).map(str::to_string),
            name: b
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            input: b.get("input").cloned().unwrap_or(Value::Null),
        })
        .collect()
}

/// True when a `user` line is the CLI echoing tool results back.
pub fn has_tool_result(msg: &MessageLine) -> bool {
    msg.blocks()
        .iter()
        .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
}

// ---------------------------------------------------------------------------
// Outbound: what we write to the CLI's stdin.
// ---------------------------------------------------------------------------

/// A user message line.
pub fn user_message(text: &str) -> Value {
    json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": text}],
        },
    })
}

/// `control_request` / `interrupt` — cancels the in-flight turn only (F5).
pub fn interrupt_request(request_id: &str) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {"subtype": "interrupt"},
    })
}

/// `control_request` / `initialize` — returns the slash command list (F9).
pub fn initialize_request(request_id: &str) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {"subtype": "initialize"},
    })
}

/// `control_request` / `set_permission_mode`.
pub fn set_permission_mode_request(request_id: &str, mode: &str) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {"subtype": "set_permission_mode", "mode": mode},
    })
}

/// The decision a human made on a [`PermissionRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow {
        #[serde(default)]
        updated_input: Option<Value>,
    },
    Deny {
        #[serde(default)]
        message: Option<String>,
    },
}

impl PermissionDecision {
    pub fn behavior(&self) -> &'static str {
        match self {
            PermissionDecision::Allow { .. } => "allow",
            PermissionDecision::Deny { .. } => "deny",
        }
    }
}

/// Our `control_response` answering a `can_use_tool` request (F2).
///
/// `original_input` is echoed back as `updatedInput` when the human did not
/// edit it, because the CLI expects the field to be present on an allow.
pub fn permission_response(
    request_id: &str,
    decision: &PermissionDecision,
    original_input: &Value,
) -> Value {
    let inner = match decision {
        PermissionDecision::Allow { updated_input } => {
            let input = updated_input
                .clone()
                .unwrap_or_else(|| original_input.clone());
            json!({
                "behavior": "allow",
                "updatedInput": if input.is_null() { json!({}) } else { input },
            })
        }
        PermissionDecision::Deny { message } => json!({
            "behavior": "deny",
            "message": message.clone().unwrap_or_else(|| "Denied by the operator".to_string()),
        }),
    };
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": inner,
        },
    })
}

/// A slash command advertised by `initialize` (F9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlashCommand {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "argumentHint", alias = "argument_hint")]
    pub argument_hint: Option<String>,
}

/// Pull the command list out of an `initialize` control response, wherever the
/// CLI happens to put it.
pub fn commands_from_initialize(payload: &Value) -> Vec<SlashCommand> {
    let candidates = ["commands", "slash_commands", "slashCommands"];
    let list = candidates
        .iter()
        .find_map(|k| payload.get(*k).and_then(Value::as_array));
    let Some(list) = list else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|v| match v {
            Value::String(name) => Some(SlashCommand {
                name: name.clone(),
                description: None,
                argument_hint: None,
            }),
            Value::Object(_) => serde_json::from_value(v.clone()).ok(),
            _ => None,
        })
        .collect()
}

/// Reject a value that would reach the CLI as its own argument and could be
/// mistaken for a flag, or that is not a plausible single token.
///
/// `--model` and `--effort` are caller-supplied and sit next to flags on the
/// command line, so they get the same treatment as a git ref.
pub fn validate_cli_value(field: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("{field} cannot be empty");
    }
    if value.starts_with('-') {
        anyhow::bail!("`{value}` starts with `-`, which the CLI would read as an option");
    }
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        anyhow::bail!("{field} may not contain whitespace or control characters");
    }
    if value.len() > 64 {
        anyhow::bail!("{field} is implausibly long");
    }
    Ok(())
}

/// Build the launch argument list for a child process.
///
/// Kept separate from spawning so it can be asserted on in tests.
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchArgs {
    pub session_id: String,
    pub resume: bool,
    pub permission_mode: crate::agent::state::PermissionMode,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub max_budget_usd: Option<f64>,
    pub add_dirs: Vec<String>,
}

impl LaunchArgs {
    pub fn to_argv(&self) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "-p".into(),
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--include-partial-messages".into(),
        ];
        args.extend(self.permission_mode.cli_flags());
        if self.resume {
            args.push("--resume".into());
            args.push(self.session_id.clone());
        } else {
            args.push("--session-id".into());
            args.push(self.session_id.clone());
        }
        if let Some(model) = &self.model {
            args.push("--model".into());
            args.push(model.clone());
        }
        if let Some(effort) = &self.effort {
            args.push("--effort".into());
            args.push(effort.clone());
        }
        if let Some(budget) = self.max_budget_usd {
            args.push("--max-budget-usd".into());
            args.push(budget.to_string());
        }
        for dir in &self.add_dirs {
            args.push("--add-dir".into());
            args.push(dir.clone());
        }
        args
    }
}

/// Serialise an outbound value as a single stdin line.
pub fn to_line(value: &Value) -> String {
    let mut s = match serde_json::to_string(value) {
        Ok(s) => s,
        // A `serde_json::Value` cannot fail to serialise; keep the line protocol
        // intact rather than propagating an impossible error.
        Err(_) => "{}".to_string(),
    };
    s.push('\n');
    s
}

/// Merge extra fields into a JSON object, for annotating stored payloads.
pub fn with_fields(base: Value, fields: &[(&str, Value)]) -> Value {
    let mut map = match base {
        Value::Object(map) => map,
        other => {
            let mut m = Map::new();
            m.insert("value".to_string(), other);
            m
        }
    };
    for (k, v) in fields {
        map.insert((*k).to_string(), v.clone());
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::state::PermissionMode;

    #[test]
    fn parses_a_system_init_line() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc","model":"opus","cwd":"/tmp","tools":["Bash","Read"],"capabilities":["interrupt_receipt_v1","interrupt_cancel_queued_v1","msg_lifecycle_v1"]}"#;
        let parsed = parse_line(line);
        let CliEvent::System(sys) = parsed.event else {
            panic!("expected system, got {:?}", parsed.event);
        };
        assert_eq!(sys.subtype.as_deref(), Some("init"));
        assert_eq!(sys.session_id.as_deref(), Some("abc"));
        assert_eq!(sys.tools, vec!["Bash", "Read"]);
        assert_eq!(sys.capabilities.len(), 3);
        // The raw line is preserved verbatim for the events table.
        assert_eq!(parsed.raw["cwd"], json!("/tmp"));
    }

    #[test]
    fn parses_assistant_text_and_tool_use() {
        let line = r#"{"type":"assistant","session_id":"abc","message":{"role":"assistant","content":[{"type":"text","text":"one"},{"type":"text","text":" two"},{"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"cargo test"}}]}}"#;
        let parsed = parse_line(line);
        let CliEvent::Assistant(msg) = parsed.event else {
            panic!("expected assistant");
        };
        assert_eq!(msg.text(), "one two");
        let uses = tool_uses(&msg);
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].name, "Bash");
        assert_eq!(uses[0].id.as_deref(), Some("tu_1"));
        assert_eq!(uses[0].label(), "Bash: cargo test");
    }

    /// Captured verbatim from `claude` 2.1.246 over `--output-format
    /// stream-json`. Note the camelCase inside `rate_limit_info`.
    #[test]
    fn parses_a_rate_limit_event() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1787745600,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"org_level_disabled","isUsingOverage":false,"unifiedWindows":{"five_hour":{"utilization":0.02,"resetsAt":1787745600},"seven_day":{"utilization":0.03,"resetsAt":1788217200}}},"uuid":"ba58a7cc","session_id":"1044"}"#;
        let parsed = parse_line(line);
        let CliEvent::RateLimit(rl) = parsed.event else {
            panic!("expected rate_limit_event, got {:?}", parsed.event);
        };
        let info = rl.rate_limit_info;
        assert_eq!(info.status, "allowed");
        assert_eq!(info.rate_limit_type.as_deref(), Some("five_hour"));
        assert_eq!(info.resets_at, Some(1787745600));
        assert_eq!(info.is_using_overage, Some(false));
        assert_eq!(info.unified_windows["five_hour"].utilization, Some(0.02));
        assert_eq!(
            info.unified_windows["seven_day"].resets_at,
            Some(1788217200)
        );
        // Fields we do not model are carried, not dropped.
        assert_eq!(info.extra["overageStatus"], json!("rejected"));
    }

    /// The account-wide minimum: `status` alone. Everything else is optional in
    /// the CLI's own schema, so nothing else may be required here either.
    #[test]
    fn a_rate_limit_event_needs_only_a_status() {
        let parsed =
            parse_line(r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"}}"#);
        let CliEvent::RateLimit(rl) = parsed.event else {
            panic!("expected rate_limit_event");
        };
        assert_eq!(rl.rate_limit_info.status, "rejected");
        assert!(rl.rate_limit_info.unified_windows.is_empty());
    }

    /// Captured verbatim from `claude` 2.1.246. Note both ids: the heartbeat's
    /// own is synthetic, and the parent is the tool actually running.
    #[test]
    fn parses_a_tool_progress_heartbeat() {
        let line = r#"{"type":"tool_progress","tool_use_id":"toolu_01XdGRi8MNS3Jmfje1yPLXPv-heartbeat-0","tool_name":"Bash","parent_tool_use_id":"toolu_01XdGRi8MNS3Jmfje1yPLXPv","elapsed_time_seconds":30,"heartbeat":true,"session_id":"688e8f88","uuid":"5250c582"}"#;
        let CliEvent::ToolProgress(prog) = parse_line(line).event else {
            panic!("expected tool_progress");
        };
        assert_eq!(prog.tool_name.as_deref(), Some("Bash"));
        assert_eq!(prog.elapsed_time_seconds, Some(30));
        assert_eq!(prog.heartbeat, Some(true));
        assert_eq!(
            prog.parent_tool_use_id.as_deref(),
            Some("toolu_01XdGRi8MNS3Jmfje1yPLXPv")
        );
        assert!(prog.subagent_retry.is_none());
    }

    #[test]
    fn parses_a_subagent_retry_and_describes_it() {
        let line = r#"{"type":"tool_progress","tool_use_id":"tu_9","tool_name":"Task","parent_tool_use_id":null,"elapsed_time_seconds":0,"subagent_type":"Explore","subagent_retry":{"agent_id":"a1","attempt":2,"max_retries":3,"retry_delay_ms":1000,"error_status":529,"error_category":"overloaded"},"uuid":"u","session_id":"s"}"#;
        let CliEvent::ToolProgress(prog) = parse_line(line).event else {
            panic!("expected tool_progress");
        };
        let retry = prog.subagent_retry.as_ref().expect("retry");
        assert_eq!(
            retry.describe(prog.subagent_type.as_deref()),
            "subagent Explore is retrying (2/3) after HTTP 529"
        );
    }

    /// A `null` `error_status` is in the CLI's schema, so the description must
    /// fall back to the category rather than printing "HTTP null".
    #[test]
    fn a_retry_without_a_status_code_names_the_category() {
        let retry = SubagentRetry {
            agent_id: None,
            attempt: None,
            max_retries: None,
            retry_delay_ms: None,
            error_status: None,
            error_category: Some("timeout".into()),
        };
        assert_eq!(
            retry.describe(None),
            "subagent subagent is retrying after timeout"
        );
    }

    #[test]
    fn tool_label_truncates_long_commands() {
        let use_ = ToolUse {
            id: None,
            name: "Bash".into(),
            input: json!({"command": "x".repeat(200)}),
        };
        let label = use_.label();
        assert!(label.starts_with("Bash: xxx"));
        assert!(label.ends_with('…'));
        assert!(label.chars().count() < 80);
    }

    #[test]
    fn string_content_is_normalised_to_a_text_block() {
        let parsed = parse_line(r#"{"type":"user","message":{"role":"user","content":"hello"}}"#);
        let CliEvent::User(msg) = parsed.event else {
            panic!("expected user");
        };
        assert_eq!(msg.text(), "hello");
        assert!(!has_tool_result(&msg));
    }

    #[test]
    fn detects_tool_results_echoed_as_user_lines() {
        let parsed = parse_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"ok"}]}}"#,
        );
        let CliEvent::User(msg) = parsed.event else {
            panic!("expected user");
        };
        assert!(has_tool_result(&msg));
    }

    #[test]
    fn parses_a_result_line_with_cost() {
        let parsed = parse_line(
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1200,"num_turns":2,"session_id":"abc","total_cost_usd":0.0421,"result":"done"}"#,
        );
        let CliEvent::Result(res) = parsed.event else {
            panic!("expected result");
        };
        assert!(!res.is_error);
        assert_eq!(res.total_cost_usd, Some(0.0421));
        assert_eq!(res.result.as_deref(), Some("done"));
    }

    #[test]
    fn parses_a_can_use_tool_control_request() {
        let line = r#"{"type":"control_request","request_id":"7","request":{"subtype":"can_use_tool","tool_name":"Write","display_name":"Write","description":"Write a file","tool_use_id":"tu_9","input":{"file_path":"/tmp/x","content":"hi"},"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Write"}]}]}}"#;
        let parsed = parse_line(line);
        let CliEvent::ControlRequest(req) = parsed.event else {
            panic!("expected control_request");
        };
        assert_eq!(req.subtype(), Some("can_use_tool"));
        let perm = req.as_permission_request().expect("permission request");
        assert_eq!(perm.request_id, "7");
        assert_eq!(perm.tool_name, "Write");
        assert_eq!(perm.tool_use_id.as_deref(), Some("tu_9"));
        assert_eq!(perm.input["file_path"], json!("/tmp/x"));
        assert!(perm.permission_suggestions.is_array());
    }

    #[test]
    fn other_control_request_subtypes_are_not_permission_prompts() {
        let parsed = parse_line(
            r#"{"type":"control_request","request_id":"1","request":{"subtype":"something_else"}}"#,
        );
        let CliEvent::ControlRequest(req) = parsed.event else {
            panic!("expected control_request");
        };
        assert!(req.as_permission_request().is_none());
    }

    #[test]
    fn parses_an_interrupt_control_response() {
        let parsed = parse_line(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"3","response":{"still_queued":["msg_a","msg_b"]}}}"#,
        );
        let CliEvent::ControlResponse(res) = parsed.event else {
            panic!("expected control_response");
        };
        assert_eq!(res.request_id(), Some("3"));
        assert!(!res.is_error());
        assert_eq!(
            res.payload().and_then(|p| p.get("still_queued")),
            Some(&json!(["msg_a", "msg_b"]))
        );
    }

    #[test]
    fn unknown_event_types_are_tolerated_not_fatal() {
        let parsed = parse_line(r#"{"type":"brand_new_thing","payload":{"a":1}}"#);
        match &parsed.event {
            CliEvent::Unknown { kind, reason } => {
                assert_eq!(kind, "brand_new_thing");
                assert!(reason.contains("unrecognised"));
            }
            other => panic!("expected unknown, got {other:?}"),
        }
        // The payload is still intact for persistence.
        assert_eq!(parsed.raw["payload"]["a"], json!(1));
    }

    #[test]
    fn malformed_json_is_tolerated() {
        let parsed = parse_line("not json at all");
        match &parsed.event {
            CliEvent::Unknown { kind, .. } => assert_eq!(kind, "<invalid json>"),
            other => panic!("expected unknown, got {other:?}"),
        }
        assert_eq!(parsed.raw["text"], json!("not json at all"));
    }

    #[test]
    fn a_known_type_with_the_wrong_shape_degrades_to_unknown() {
        let parsed = parse_line(r#"{"type":"assistant","session_id":"abc"}"#);
        match &parsed.event {
            CliEvent::Unknown { kind, .. } => assert_eq!(kind, "assistant"),
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    #[test]
    fn a_line_without_a_type_is_unknown() {
        let parsed = parse_line(r#"{"hello":"world"}"#);
        assert!(matches!(parsed.event, CliEvent::Unknown { .. }));
    }

    #[test]
    fn stream_events_keep_their_payload() {
        let parsed =
            parse_line(r#"{"type":"stream_event","event":{"type":"content_block_delta"}}"#);
        let CliEvent::StreamEvent(v) = &parsed.event else {
            panic!("expected stream_event");
        };
        assert_eq!(v["event"]["type"], json!("content_block_delta"));
    }

    #[test]
    fn outbound_lines_round_trip_through_the_parser() {
        for value in [
            user_message("hello"),
            interrupt_request("1"),
            initialize_request("2"),
            set_permission_mode_request("3", "acceptEdits"),
        ] {
            let line = to_line(&value);
            assert!(line.ends_with('\n'));
            let back: Value = serde_json::from_str(line.trim_end()).expect("valid json");
            assert_eq!(back, value);
        }
    }

    #[test]
    fn interrupt_request_shape_matches_the_cli() {
        assert_eq!(
            interrupt_request("42"),
            json!({"type":"control_request","request_id":"42","request":{"subtype":"interrupt"}})
        );
    }

    #[test]
    fn permission_allow_echoes_the_original_input() {
        let original = json!({"file_path": "/tmp/x"});
        let value = permission_response(
            "7",
            &PermissionDecision::Allow {
                updated_input: None,
            },
            &original,
        );
        assert_eq!(value["type"], json!("control_response"));
        assert_eq!(value["response"]["request_id"], json!("7"));
        assert_eq!(value["response"]["response"]["behavior"], json!("allow"));
        assert_eq!(value["response"]["response"]["updatedInput"], original);
    }

    #[test]
    fn permission_allow_can_override_the_input() {
        let value = permission_response(
            "7",
            &PermissionDecision::Allow {
                updated_input: Some(json!({"file_path": "/tmp/y"})),
            },
            &json!({"file_path": "/tmp/x"}),
        );
        assert_eq!(
            value["response"]["response"]["updatedInput"]["file_path"],
            json!("/tmp/y")
        );
    }

    #[test]
    fn permission_deny_carries_a_message() {
        let value = permission_response(
            "7",
            &PermissionDecision::Deny {
                message: Some("no".into()),
            },
            &Value::Null,
        );
        assert_eq!(value["response"]["response"]["behavior"], json!("deny"));
        assert_eq!(value["response"]["response"]["message"], json!("no"));
    }

    #[test]
    fn permission_decision_json_round_trips() {
        let d = PermissionDecision::Allow {
            updated_input: Some(json!({"a": 1})),
        };
        let text = serde_json::to_string(&d).expect("serialise");
        let back: PermissionDecision = serde_json::from_str(&text).expect("parse");
        assert_eq!(d, back);
        assert_eq!(back.behavior(), "allow");

        let d = PermissionDecision::Deny { message: None };
        let text = serde_json::to_string(&d).expect("serialise");
        let back: PermissionDecision = serde_json::from_str(&text).expect("parse");
        assert_eq!(d, back);
    }

    #[test]
    fn initialize_command_list_is_extracted() {
        let payload = json!({
            "commands": [
                {"name": "/compact", "description": "Compact the conversation", "argumentHint": "[instructions]"},
                {"name": "/bare"},
                "/plain-string"
            ]
        });
        let cmds = commands_from_initialize(&payload);
        assert_eq!(cmds.len(), 3);
        assert_eq!(cmds[0].name, "/compact");
        assert_eq!(cmds[0].argument_hint.as_deref(), Some("[instructions]"));
        assert_eq!(cmds[1].description, None);
        assert_eq!(cmds[2].name, "/plain-string");
        assert!(commands_from_initialize(&json!({})).is_empty());
    }

    #[test]
    fn first_launch_uses_session_id_and_resume_replaces_it() {
        let base = LaunchArgs {
            session_id: "uuid-1".into(),
            resume: false,
            permission_mode: PermissionMode::Ask,
            model: Some("opus".into()),
            effort: None,
            max_budget_usd: None,
            add_dirs: vec![],
        };
        let argv = base.to_argv();
        assert!(argv.starts_with(&[
            "-p".to_string(),
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--include-partial-messages".into(),
        ]));
        assert!(argv.windows(2).any(|w| w == ["--session-id", "uuid-1"]));
        assert!(!argv.iter().any(|a| a == "--resume"));
        assert!(
            argv.windows(2)
                .any(|w| w == ["--permission-prompt-tool", "stdio"])
        );

        let resumed = LaunchArgs {
            resume: true,
            ..base
        }
        .to_argv();
        assert!(resumed.windows(2).any(|w| w == ["--resume", "uuid-1"]));
        assert!(!resumed.iter().any(|a| a == "--session-id"));
    }

    #[test]
    fn optional_launch_flags_are_only_emitted_when_set() {
        let args = LaunchArgs {
            session_id: "u".into(),
            resume: false,
            permission_mode: PermissionMode::Dangerous,
            model: None,
            effort: Some("high".into()),
            max_budget_usd: Some(2.5),
            add_dirs: vec!["/a".into(), "/b".into()],
        };
        let argv = args.to_argv();
        assert!(!argv.iter().any(|a| a == "--model"));
        assert!(argv.windows(2).any(|w| w == ["--effort", "high"]));
        assert!(argv.windows(2).any(|w| w == ["--max-budget-usd", "2.5"]));
        assert_eq!(argv.iter().filter(|a| *a == "--add-dir").count(), 2);
        assert!(argv.iter().any(|a| a == "--dangerously-skip-permissions"));
        assert!(!argv.iter().any(|a| a == "--permission-mode"));
    }

    #[test]
    fn event_kind_strings_round_trip() {
        for k in [
            EventKind::User,
            EventKind::Assistant,
            EventKind::ToolUse,
            EventKind::ToolResult,
            EventKind::PermissionRequest,
            EventKind::PermissionDecision,
            EventKind::System,
            EventKind::Result,
            EventKind::Stderr,
            EventKind::Error,
        ] {
            assert_eq!(k.as_str().parse::<EventKind>().expect("parse"), k);
        }
        assert!("nope".parse::<EventKind>().is_err());
    }

    #[test]
    fn with_fields_annotates_objects_and_wraps_scalars() {
        let v = with_fields(json!({"a": 1}), &[("b", json!(2))]);
        assert_eq!(v, json!({"a": 1, "b": 2}));
        let v = with_fields(json!("scalar"), &[("b", json!(2))]);
        assert_eq!(v, json!({"value": "scalar", "b": 2}));
    }

    #[test]
    fn option_shaped_model_and_effort_are_rejected() {
        for bad in [
            "--dangerously-skip-permissions",
            "-x",
            "",
            "with space",
            "line\nbreak",
            &"x".repeat(65),
        ] {
            assert!(validate_cli_value("model", bad).is_err(), "{bad:?}");
        }
        for good in ["opus", "claude-sonnet-4-5", "high", "v1.2"] {
            assert!(validate_cli_value("model", good).is_ok(), "{good:?}");
        }
    }
}
