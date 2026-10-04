//! Wire models for the vibe app-server protocol.
//!
//! Mirrors `vibe/app_server/models.py` + `protocol.py`: ProtocolModel fields are
//! camelCase aliases of snake_case members; StrEnum members serialize as their
//! lowercase names. Discriminated unions use the same tag fields as the wire
//! (`type`, `kind`, `role`, `status`, `transport`, `op`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Envelope (protocol.py Notification / ServerRequest / responses)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Error payload inside a JSON-RPC error response (ProtocolError).
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "camelCase")]
#[error("{code}: {message}")]
pub struct ProtocolErrorBody {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub data: Value,
}

/// Any message the server can send us.
#[derive(Debug, Clone)]
pub enum ServerMessage {
    Notification {
        method: String,
        params: Value,
    },
    /// A request the server expects an answer to (e.g. `callback/call`).
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawIncoming {
    Method {
        method: String,
        id: Option<Value>,
        params: Option<Value>,
    },
    // `result` must be required: an error envelope has an `id` and no
    // `result`, so an optional field would swallow the error as `null`.
    Result {
        id: Value,
        result: Value,
    },
    Error {
        id: Value,
        error: ProtocolErrorBody,
    },
}

pub enum Incoming {
    Message(ServerMessage),
    Response {
        id: Value,
        result: Result<Value, ProtocolErrorBody>,
    },
}

pub fn parse_incoming(line: &str) -> Result<Incoming, serde_json::Error> {
    let raw: RawIncoming = serde_json::from_str(line)?;
    Ok(match raw {
        RawIncoming::Method { method, id, params } => Incoming::Message(match id {
            Some(id) => ServerMessage::Request {
                id,
                method,
                params: params.unwrap_or(Value::Null),
            },
            None => ServerMessage::Notification {
                method,
                params: params.unwrap_or(Value::Null),
            },
        }),
        RawIncoming::Result { id, result } => Incoming::Response {
            id,
            result: Ok(result),
        },
        RawIncoming::Error { id, error } => Incoming::Response {
            id,
            result: Err(error),
        },
    })
}

// ---------------------------------------------------------------------------
// initialize
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default = "default_entrypoint")]
    pub entrypoint: String,
    #[serde(default)]
    pub terminal_emulator: String,
}

fn default_entrypoint() -> String {
    "unknown".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(default)]
    pub callback_kinds: Vec<String>,
    #[serde(default)]
    pub client_tools: Vec<String>,
    #[serde(default)]
    pub disabled_notifications: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub client_info: ClientInfo,
    #[serde(default)]
    pub capabilities: ClientCapabilities,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub server_info: ServerInfo,
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionConfig {
    #[serde(rename = "type")]
    pub type_: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<CompletionConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default)]
    pub auto_approve: bool,
    #[serde(default)]
    pub headless: bool,
    #[serde(default)]
    pub trust_workspace: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartParams {
    #[serde(default)]
    pub agent_config: AgentConfig,
    pub history_limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResumeParams {
    pub session_id: String,
    #[serde(default)]
    pub agent_config: AgentConfig,
    pub history_limit: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionContinueParams {
    #[serde(default)]
    pub agent_config: AgentConfig,
    pub history_limit: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReadParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<PageRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns: Option<PageRequest>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    pub limit: u32,
    #[serde(default = "backward")]
    pub direction: String,
}

fn backward() -> String {
    "backward".to_string()
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            cursor: None,
            limit: 200,
            direction: backward(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListResponse {
    #[serde(default)]
    pub items: Vec<PublicSession>,
    pub next_cursor: Option<String>,
    pub previous_cursor: Option<String>,
    pub continue_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionArchiveParams {
    pub session_id: String,
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPinParams {
    pub session_id: String,
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTitleUpdateParams {
    pub session_id: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionShellCommandParams {
    pub session_id: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactParams {
    pub session_id: String,
    #[serde(default)]
    pub extra_instructions: String,
}

// ---------------------------------------------------------------------------
// Public projections (models.py)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSummary {
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub safety: String,
    #[serde(default)]
    pub agent_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PublicSessionStatus {
    Idle,
    Running {
        active_turn_id: String,
    },
    Blocked {
        active_turn_id: String,
        callback_id: String,
        reason: String,
    },
    Failed {
        message: String,
    },
    Archived,
    #[serde(other)]
    Unknown,
}

impl PublicSessionStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running { .. } => "running",
            Self::Blocked { .. } => "blocked",
            Self::Failed { .. } => "failed",
            Self::Archived => "archived",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicSession {
    pub id: String,
    pub root_session_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub title: Option<String>,
    #[serde(default)]
    pub preview: String,
    pub status: PublicSessionStatus,
    pub created_at: u64,
    pub updated_at: u64,
    pub bumped_at: Option<u64>,
    pub pinned_at: Option<u64>,
    pub archived_at: Option<u64>,
    #[serde(default)]
    pub is_unseen: bool,
    pub cwd: Option<String>,
    #[serde(default)]
    pub workspace_roots: Vec<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub agent: Option<AgentSummary>,
    pub token_usage: Option<TokenUsage>,
    pub context_usage: Option<TokenUsage>,
    pub harness: Option<String>,
}

impl PublicSession {
    pub fn display_title(&self) -> String {
        if let Some(title) = &self.title {
            if !title.is_empty() {
                return title.clone();
            }
        }
        if !self.preview.is_empty() {
            return self.preview.clone();
        }
        format!("session {}", &self.id[..self.id.len().min(8)])
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicChildSession {
    pub id: String,
    pub name: String,
    pub agent_type: String,
    pub status: PublicSessionStatus,
    #[serde(default)]
    pub token_usage: TokenUsage,
    pub context_usage: Option<TokenUsage>,
    pub created_at: u64,
    pub updated_at: u64,
}

// --- Content ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileImageSource {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineImageSource {
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageSource {
    File {
        path: String,
    },
    Inline {
        data: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageAttachment {
    pub source: ImageSource,
    pub alias: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        attachment: ImageAttachment,
    },
    Resource {
        resource: Value,
    },
    #[serde(other)]
    Unknown,
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserDisplayContent {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Value>,
}

// --- Turn input ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum TurnInputEntry {
    User {
        #[serde(default)]
        entry_id: Option<String>,
        content: Vec<SessionContentBlock>,
    },
    Context {
        #[serde(default)]
        entry_id: Option<String>,
        content: Vec<SessionContentBlock>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionContentBlock {
    Text {
        text: String,
    },
    Image {
        uri: String,
        media_type: Option<String>,
        alt_text: Option<String>,
    },
    ResourceLink {
        uri: String,
        name: Option<String>,
        title: Option<String>,
        description: Option<String>,
        media_type: Option<String>,
        size: Option<u64>,
    },
    EmbeddedResource {
        uri: String,
        media_type: Option<String>,
        text: Option<String>,
        blob: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

// --- History entries --------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicError {
    pub message: String,
    pub code: Option<String>,
    #[serde(default)]
    pub details: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectCallDisplay {
    pub summary: String,
    pub content: Option<String>,
    #[serde(default)]
    pub suffix: String,
    #[serde(default)]
    pub verb: String,
    pub message: Option<String>,
    #[serde(default)]
    pub settled_verb: String,
    pub settled_message: Option<String>,
    #[serde(default)]
    pub status_text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectResultDisplay {
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub verb: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub approval_note: Option<String>,
    #[serde(default)]
    pub suffix: String,
}

/// Effect detail union — the per-kind `input` stays untyped; every variant
/// shares `toolName`/`display` so a single struct covers the wire shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectDetail {
    pub kind: String,
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub display: Option<EffectCallDisplay>,
    #[serde(default)]
    pub input: Option<Value>,
    #[serde(default)]
    pub child_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EffectState {
    Pending,
    Running {
        #[serde(default)]
        output_text: String,
    },
    Blocked {
        callback_id: String,
        #[serde(default)]
        output_text: String,
    },
    Completed {
        #[serde(default)]
        output: Value,
        #[serde(default)]
        output_text: String,
        #[serde(default)]
        duration_ms: f64,
        #[serde(default)]
        display: Option<EffectResultDisplay>,
        #[serde(default)]
        decision: Option<String>,
        #[serde(default)]
        approval_type: Option<String>,
        #[serde(default)]
        approval_source: Option<String>,
    },
    Failed {
        error: PublicError,
        #[serde(default)]
        output: Value,
        #[serde(default)]
        output_text: String,
        #[serde(default)]
        duration_ms: f64,
        #[serde(default)]
        display: Option<EffectResultDisplay>,
    },
    Cancelled {
        #[serde(default)]
        reason: String,
        #[serde(default)]
        output_text: String,
        #[serde(default)]
        duration_ms: f64,
        #[serde(default)]
        display: Option<EffectResultDisplay>,
    },
    Skipped {
        #[serde(default)]
        reason: String,
        #[serde(default)]
        display: Option<EffectResultDisplay>,
    },
    #[serde(other)]
    Unknown,
}

impl EffectState {
    pub fn status(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running { .. } => "running",
            Self::Blocked { .. } => "blocked",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::Cancelled { .. } => "cancelled",
            Self::Skipped { .. } => "skipped",
            Self::Unknown => "unknown",
        }
    }

    pub fn is_settled(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. }
                | Self::Skipped { .. }
        )
    }
}

// --- Callbacks --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequiredPermission {
    pub scope: String,
    #[serde(default)]
    pub invocation_pattern: String,
    #[serde(default)]
    pub session_pattern: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub path_scope_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalCallbackDetail {
    pub effect: Option<EffectDetail>,
    #[serde(default)]
    pub required_permissions: Vec<RequiredPermission>,
    #[serde(default)]
    pub choices: Vec<String>,
    #[serde(default)]
    pub path_scope_choices: Vec<String>,
    pub related_entry_id: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionChoice {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserQuestion {
    pub question: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub options: Vec<QuestionChoice>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default)]
    pub hide_other: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserQuestionRequest {
    #[serde(default)]
    pub questions: Vec<UserQuestion>,
    pub footer_note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputCallbackDetail {
    pub request: Option<UserQuestionRequest>,
    pub related_entry_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CallbackDetail {
    Approval(Box<ApprovalCallbackDetail>),
    UserInput(UserInputCallbackDetail),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserAnswer {
    pub question: String,
    pub answer: String,
    #[serde(default)]
    pub is_other: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserQuestionResult {
    #[serde(default)]
    pub answers: Vec<UserAnswer>,
    #[serde(default)]
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CallbackOutput {
    Approval {
        decision: ApprovalDecision,
        #[serde(default)]
        feedback: Option<String>,
    },
    UserInput {
        result: UserQuestionResult,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDecision {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_scope: Option<String>,
}

impl ApprovalDecision {
    pub const APPROVE: &'static str = "approve";
    pub const APPROVE_FOR_SESSION: &'static str = "approve_for_session";
    pub const APPROVE_PERMANENTLY: &'static str = "approve_permanently";
    pub const DENY: &'static str = "deny";
    pub const CANCEL_TURN: &'static str = "cancel_turn";

    pub fn of(type_: &str) -> Self {
        Self {
            type_: type_.to_string(),
            path_scope: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CallbackState {
    Open,
    Answered {
        output: CallbackOutput,
    },
    Cancelled {
        reason: String,
    },
    Expired {
        reason: String,
    },
    #[serde(other)]
    Unknown,
}

// --- Entries -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntryBase {
    pub id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub generation_status: String,
    pub related_entry_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
// Wire mirror: variants carry full payloads by design.
#[allow(clippy::large_enum_variant)]
pub enum PublicHistoryEntry {
    Message {
        #[serde(flatten)]
        base: HistoryEntryBase,
        role: String,
        #[serde(default)]
        content: Vec<ContentBlock>,
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        user_display_content: Option<UserDisplayContent>,
    },
    Reasoning {
        #[serde(flatten)]
        base: HistoryEntryBase,
        #[serde(default)]
        text: String,
        #[serde(default)]
        summary: Vec<String>,
    },
    Effect {
        #[serde(flatten)]
        base: HistoryEntryBase,
        #[serde(default)]
        title: String,
        detail: EffectDetail,
        state: EffectState,
    },
    Callback {
        #[serde(flatten)]
        base: HistoryEntryBase,
        callback_id: String,
        #[serde(default)]
        title: String,
        detail: CallbackDetail,
        state: CallbackState,
    },
    Checkpoint {
        #[serde(flatten)]
        base: HistoryEntryBase,
        #[serde(default)]
        kind: String,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        details: Value,
    },
    Notice {
        #[serde(flatten)]
        base: HistoryEntryBase,
        #[serde(default)]
        level: String,
        #[serde(default)]
        message: String,
        #[serde(default)]
        detail: Option<Value>,
    },
    #[serde(other)]
    Unknown,
}

impl PublicHistoryEntry {
    pub fn base(&self) -> Option<&HistoryEntryBase> {
        match self {
            Self::Message { base, .. }
            | Self::Reasoning { base, .. }
            | Self::Effect { base, .. }
            | Self::Callback { base, .. }
            | Self::Checkpoint { base, .. }
            | Self::Notice { base, .. } => Some(base),
            Self::Unknown => None,
        }
    }

    /// None for `Unknown` entries (they carry no base) — callers must not
    /// assume every entry has an id.
    pub fn id(&self) -> Option<&str> {
        self.base().map(|b| b.id.as_str())
    }

    /// For `Callback` entries: the wire callback id (`cb-…`) used by
    /// `callback/respond`. Distinct from `id()` (the history-entry id).
    pub fn callback_id(&self) -> Option<&str> {
        match self {
            Self::Callback { callback_id, .. } => Some(callback_id),
            _ => None,
        }
    }
}

// --- Session state ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicQueuedTurn {
    pub id: String,
    pub created_at: u64,
    #[serde(default)]
    pub entries: Vec<TurnInputEntry>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicTurnQueue {
    #[serde(default)]
    pub items: Vec<PublicQueuedTurn>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub max_items: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicTurn {
    pub id: String,
    pub session_id: String,
    pub status: String,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub error: Option<PublicError>,
    pub stop_reason: Option<String>,
    pub queue_item_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicRetryState {
    pub turn_id: String,
    pub category: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicSessionState {
    #[serde(default)]
    pub format: String,
    pub event_id: u64,
    pub session: PublicSession,
    pub is_quiescent: Option<bool>,
    pub history: Option<Vec<PublicHistoryEntry>>,
    pub history_before_cursor: Option<String>,
    pub turns: Option<Vec<PublicTurn>>,
    #[serde(default)]
    pub active_callbacks: Vec<PublicHistoryEntry>,
    #[serde(default)]
    pub child_sessions: Vec<PublicChildSession>,
    #[serde(default)]
    pub turn_queue: PublicTurnQueue,
    #[serde(default)]
    pub retrying: Option<PublicRetryState>,
}

// --- Stats / runtime -----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatsSnapshot {
    #[serde(default)]
    pub steps: u64,
    #[serde(default)]
    pub session_prompt_tokens: u64,
    #[serde(default)]
    pub session_completion_tokens: u64,
    #[serde(default)]
    pub session_cached_tokens: u64,
    #[serde(default)]
    pub context_tokens: u64,
    #[serde(default)]
    pub last_turn_duration: f64,
    #[serde(default)]
    pub tokens_per_second: f64,
}

// --- Turns ----------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub session_id: String,
    pub message: Vec<ContentBlock>,
    #[serde(default)]
    pub injected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_user_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResponse {
    pub turn: PublicTurn,
    #[serde(default)]
    pub last_event_id: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerParams {
    pub session_id: String,
    pub expected_turn_id: String,
    pub message: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    pub session_id: String,
    pub expected_turn_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnEnqueueParams {
    pub session_id: String,
    pub entries: Vec<TurnInputEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnQueueRemoveParams {
    pub session_id: String,
    pub queue_item_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueItemResponse {
    pub queue_item_id: String,
}

// --- Callbacks wire ---------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallbackCallParams {
    pub callback: PublicHistoryEntry,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallbackCallResponse {
    pub callback_id: String,
    pub accepted: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallbackRespondParams {
    pub session_id: String,
    pub callback_id: String,
    pub output: CallbackOutput,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallbackRespondResponse {
    pub status: String,
}

// --- Notifications ------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventNotificationParams {
    pub event_id: u64,
    pub session_id: String,
    #[serde(default)]
    pub emitted_at: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntryAddedParams {
    #[serde(flatten)]
    pub base: EventNotificationParams,
    pub turn_id: Option<String>,
    pub entry: PublicHistoryEntry,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntryUpdatedParams {
    #[serde(flatten)]
    pub base: EventNotificationParams,
    pub turn_id: Option<String>,
    pub entry_id: String,
    #[serde(default)]
    pub patch: Vec<JsonPatchOperation>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshotParams {
    #[serde(flatten)]
    pub base: EventNotificationParams,
    pub state: PublicSessionState,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHandoffParams {
    #[serde(flatten)]
    pub base: EventNotificationParams,
    pub old_session_id: String,
    pub state: PublicSessionState,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdatedParams {
    #[serde(flatten)]
    pub base: EventNotificationParams,
    #[serde(default)]
    pub patch: Vec<JsonPatchOperation>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedParams {
    #[serde(flatten)]
    pub base: EventNotificationParams,
    pub turn: PublicTurn,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedParams {
    #[serde(flatten)]
    pub base: EventNotificationParams,
    pub turn: PublicTurn,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnQueueUpdatedParams {
    #[serde(flatten)]
    pub base: EventNotificationParams,
    pub queue: PublicTurnQueue,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsUpdatedParams {
    #[serde(flatten)]
    pub base: EventNotificationParams,
    pub stats: AgentStatsSnapshot,
    #[serde(default)]
    pub context_window: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildSessionUpdatedParams {
    #[serde(flatten)]
    pub base: EventNotificationParams,
    pub child_session: PublicChildSession,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnRetryingParams {
    pub session_id: String,
    pub category: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerWarningParams {
    pub warning: PublicError,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerErrorParams {
    pub error: PublicError,
}

// --- JSON Patch -----------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonPatchOperation {
    pub op: String,
    pub path: String,
    #[serde(default)]
    pub value: Value,
}

// --- Runtime snapshot (runtime/updated) -----------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeUpdatedParams {
    pub session_id: String,
    pub runtime: RuntimeSnapshot,
}

/// runtime/read + runtime/updated carry a large snapshot; for M1 keep the
/// fields the status bar needs and retain the rest untyped.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSnapshot {
    #[serde(default)]
    pub active_agent: Option<AgentSummary>,
    #[serde(default)]
    pub agents: Vec<AgentSummary>,
    #[serde(default)]
    pub stats: AgentStatsSnapshot,
    #[serde(default)]
    pub context_window: u64,
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub issues: Vec<Value>,
}

// --- M2: queue / rewind / history / trust ------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnQueueReadParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnQueueReadResponse {
    pub queue: PublicTurnQueue,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnQueueResumeParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnQueueSteerParams {
    pub session_id: String,
    pub queue_item_id: String,
    pub expected_turn_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnQueueSteerResponse {
    pub queue_item_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnQueueReplaceParams {
    pub session_id: String,
    pub queue_item_id: String,
    pub entries: Vec<TurnInputEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRewindReadParams {
    pub session_id: String,
    pub entry_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRewindReadResponse {
    pub has_file_changes: bool,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRewindParams {
    pub session_id: String,
    pub entry_id: String,
    #[serde(default)]
    pub restore_files: bool,
    #[serde(default)]
    pub inplace: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRewindResponse {
    pub message: String,
    #[serde(default)]
    pub restore_errors: Vec<String>,
    #[serde(default)]
    pub restored_paths: Vec<String>,
    pub state: PublicSessionState,
    #[serde(default)]
    pub session_log: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHistoryListParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub page: PageRequest,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHistoryListResponse {
    #[serde(default)]
    pub items: Vec<PublicHistoryEntry>,
    pub next_cursor: Option<String>,
    pub previous_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTrustStatusParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTrustDetails {
    pub cwd: String,
    pub repo_root: Option<String>,
    #[serde(default)]
    pub detected_files: Vec<String>,
    #[serde(default)]
    pub repo_detected_files: Vec<String>,
    #[serde(default)]
    pub repo_explicitly_untrusted: bool,
    #[serde(default)]
    pub settings_path: String,
    #[serde(default)]
    pub available_decisions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTrustStatusResponse {
    pub status: String,
    pub details: Option<WorkspaceTrustDetails>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTrustDecisionParams {
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceUntrustedConfigParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceUntrustedConfigResponse {
    #[serde(default)]
    pub dirs: Vec<String>,
    #[serde(default)]
    pub settings_path: String,
}

// --- M2b: voice --------------------------------------------------------------

/// `config/read` audio provider view (`AudioProviderView` upstream).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioProviderView {
    pub api_base: String,
    #[serde(default)]
    pub api_key_env_var: String,
    #[serde(default)]
    pub client: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscribeModelConfigView {
    pub name: String,
    #[serde(default)]
    pub sample_rate: u32,
    #[serde(default)]
    pub encoding: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub target_streaming_delay_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptionConfigView {
    pub model: TranscribeModelConfigView,
    pub provider: AudioProviderView,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TtsModelConfigView {
    pub name: String,
    #[serde(default)]
    pub voice: String,
    #[serde(default)]
    pub response_format: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechConfigView {
    pub model: TtsModelConfigView,
    pub provider: AudioProviderView,
}

/// `config/read` config view — the subset the client renders, tolerant to
/// upstream growth (ADR-0014 version-skew rule: deserialize only what the
/// client uses).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigView {
    #[serde(default)]
    pub voice_mode_enabled: bool,
    #[serde(default)]
    pub narrator_enabled: bool,
    /// Voice sub-views are optional — a server without speech/
    /// transcription config must not sink the whole read (ADR-0014 skew).
    pub speech: Option<SpeechConfigView>,
    pub transcription: Option<TranscriptionConfigView>,
    // --- M3a: settings ---
    #[serde(default)]
    pub active_model: ModelConfigView,
    #[serde(default)]
    pub active_model_pinned: bool,
    #[serde(default)]
    pub default_model_alias: String,
    #[serde(default)]
    pub default_agent: String,
    #[serde(default)]
    pub models: Vec<ModelConfigView>,
    #[serde(default)]
    pub theme: String,
    #[serde(default)]
    pub worktree_limit: u32,
    #[serde(default)]
    pub enable_notifications: bool,
    #[serde(default)]
    pub transcribe_models: Vec<String>,
    #[serde(default)]
    pub tts_models: Vec<String>,
    #[serde(default)]
    pub validation_warnings: Vec<String>,
}

/// `config/read` response — the client reads `config` as `ConfigView`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigReadResponse {
    pub config: ConfigView,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NarrationSummarizeParams {
    pub session_id: String,
    pub user_message: String,
    pub assistant_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NarrationSummarizeResponse {
    #[serde(default)]
    pub summary: Option<String>,
}

// --- M3a: settings & pickers --------------------------------------------

/// `models[]` / `activeModel` entry in `ConfigView` (`ModelConfigView`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfigView {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub alias: String,
    /// "off" | "low" | "medium" | "high" | "max"
    #[serde(default)]
    pub thinking: String,
    #[serde(default)]
    pub supports_images: bool,
    #[serde(default)]
    pub display_name: String,
}

/// `agents/list` response — `AgentSummary` is defined with the session
/// models (`safety`/`agent_type` stay strings so new variants decode).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentsListResponse {
    pub active: AgentSummary,
    #[serde(default)]
    pub agents: Vec<AgentSummary>,
}

/// One layer's contribution to a config field (`ConfigLayerValueWire`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigLayerValueView {
    #[serde(default)]
    pub layer: String,
    #[serde(default)]
    pub value: serde_json::Value,
}

/// `config/fields/read` field (`ConfigFieldWire`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFieldView {
    pub name: String,
    /// "bool" | "enum" | "int" | "float" | "str" | "list" | "complex"
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub value: serde_json::Value,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub popular: bool,
    #[serde(default)]
    pub enum_choices: Vec<String>,
    #[serde(default)]
    pub value_labels: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub layer_values: Vec<ConfigLayerValueView>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFieldsReadResponse {
    #[serde(default)]
    pub fields: Vec<ConfigFieldView>,
    #[serde(default)]
    pub targets: Vec<String>,
}

/// `config/write` op (`ConfigWriteOpWire`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigWriteOp {
    /// "set" | "remove"
    pub op: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_layer: Option<String>,
}

/// Mutation acknowledgement — `RuntimeMutationResponse`'s `status`
/// ("applied"/"pending") plus `ConfigWriteResponse`'s rejection fields;
/// the embedded runtime snapshot is ignored (tolerant).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigWriteResponse {
    #[serde(default)]
    pub rejected: bool,
    #[serde(default)]
    pub failures: Vec<String>,
    #[serde(default)]
    pub status: Option<String>,
    /// `RuntimeMutationResponse` embeds a `runtime` snapshot — kept as a
    /// loose `Value` so `activeAgent` etc. can be read tolerantly.
    #[serde(default)]
    pub runtime: Option<serde_json::Value>,
}

/// `config/model/write` params (`ModelConfigWriteParams`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfigWriteParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// `session/agent/update` params (`AgentSwitchParams`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSwitchParams {
    pub session_id: String,
    pub agent_name: String,
}

// --- M3b: review diff ----------------------------------------------------

/// Who produced a review scope (`ReviewOwner` upstream — discriminated by
/// `kind`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum ReviewOwner {
    Agent { turn_id: i64 },
    Manual { index: i64 },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewRegionRef {
    #[serde(default)]
    pub version_index: u32,
    #[serde(default)]
    pub ordinal: u32,
}

/// `ReviewRegion` upstream — text regions carry line ranges; `decision` is
/// "pending" | "keep" | "revert".
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum ReviewRegion {
    Text {
        version_index: u32,
        ordinal: u32,
        owner: ReviewOwner,
        #[serde(default)]
        baseline_start: u32,
        #[serde(default)]
        baseline_line_count: u32,
        #[serde(default)]
        current_start: u32,
        #[serde(default)]
        current_line_count: u32,
        decision: String,
        #[serde(default)]
        depends_on: Vec<ReviewRegionRef>,
    },
    Opaque {
        version_index: u32,
        ordinal: u32,
        owner: ReviewOwner,
        #[serde(default)]
        reason: Option<String>,
        decision: String,
        #[serde(default)]
        depends_on: Vec<ReviewRegionRef>,
    },
}

impl ReviewRegion {
    /// Who authored the region — both variants carry one.
    pub fn owner(&self) -> Option<ReviewOwner> {
        match self {
            ReviewRegion::Text { owner, .. } | ReviewRegion::Opaque { owner, .. } => {
                Some(owner.clone())
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewFile {
    pub path: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub regions: Vec<ReviewRegion>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewScopeFile {
    pub path: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub region_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewScope {
    pub owner: ReviewOwner,
    #[serde(default)]
    pub files: Vec<ReviewScopeFile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewStateResponse {
    #[serde(default)]
    pub files: Vec<ReviewFile>,
    #[serde(default)]
    pub scopes: Vec<ReviewScope>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewTurnDiffResponse {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub baseline: String,
    #[serde(default)]
    pub current: String,
}

/// `review/approve|revert` target (`ReviewTarget` upstream, `kind`-tagged).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum ReviewTarget {
    Region {
        path: String,
        version_index: u32,
        ordinal: u32,
    },
    Regions {
        path: String,
        regions: Vec<ReviewTargetRegionRef>,
    },
    Scope {
        owner: ReviewOwner,
    },
    ScopeFile {
        owner: ReviewOwner,
        path: String,
    },
    File {
        path: String,
    },
    All {},
    LastTurns {
        count: u32,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewTargetRegionRef {
    pub version_index: u32,
    pub ordinal: u32,
}

// ---------------------------------------------------------------------------
// M3c: extensions — skills, MCP catalog, connectors, plugins
// (models.py SkillSummary / MCPState / ConnectorCounts / PluginCatalog*,
// protocol.py SkillsSetEnabledParams / MCPToggleParams / read responses)
// ---------------------------------------------------------------------------

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillSummary {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_true")]
    pub user_invocable: bool,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub locked: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillsInstalledResponse {
    #[serde(default)]
    pub skills: Vec<SkillSummary>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillsSetEnabledParams {
    pub session_id: String,
    pub name: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MCPToolSummary {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub enabled: bool,
}

/// `kind`: "server" | "connector"; `status`: "disabled" | "connected" |
/// "enabled" | "needs_auth" | "needs_setup" | "unavailable" (String per
/// ADR-0014 skew).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MCPSourceSummary {
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub transport: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub tools: Vec<MCPToolSummary>,
    pub error: Option<String>,
    pub plugin_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MCPState {
    #[serde(default)]
    pub sources: Vec<MCPSourceSummary>,
    #[serde(default)]
    pub discovery_errors: std::collections::HashMap<String, String>,
    pub connector_error: Option<String>,
    pub manage_connectors_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MCPReadResponse {
    pub mcp: MCPState,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MCPToggleParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub name: String,
    pub source: String,
    pub disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

/// `mcp/toggle` answers `MCPCatalogMutationResponse{runtime}` — the fresh
/// `MCPState` lives at `runtime.mcp`.
#[derive(Debug, Clone, Deserialize)]
pub struct MCPMutationResponse {
    pub runtime: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConnectorCounts {
    #[serde(default)]
    pub connected: u32,
    pub total: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConnectorsReadResponse {
    pub counts: ConnectorCounts,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCatalogComponent {
    #[serde(default)]
    pub kind: String,
    pub name: String,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCatalogEntry {
    pub name: String,
    pub version: Option<String>,
    #[serde(default)]
    pub source_format: String,
    #[serde(default)]
    pub description: String,
    pub author: Option<String>,
    pub scope: Option<String>,
    #[serde(default)]
    pub components: Vec<PluginCatalogComponent>,
    #[serde(default)]
    pub drifted: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginCatalogDropped {
    pub file: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginCatalogState {
    #[serde(default)]
    pub plugins: Vec<PluginCatalogEntry>,
    #[serde(default)]
    pub dropped: Vec<PluginCatalogDropped>,
}

/// `plugins/read` wraps the catalog state one level: `{plugins: {plugins,
/// dropped}}`.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginsReadResponse {
    pub plugins: PluginCatalogState,
}

// ---------------------------------------------------------------------------
// M3d: workspace — worktrees + scheduled loops
// (protocol.py Workspace*Worktree* / Loops*; models.py ScheduledLoop)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceGitBranchChanges {
    #[serde(default)]
    pub additions: u32,
    #[serde(default)]
    pub deletions: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceLinkedWorktree {
    pub name: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub repo_root: String,
    pub branch_changes: Option<WorkspaceGitBranchChanges>,
}

/// `workspace/git/worktrees/list` — workspace-scoped (keyed by `cwd`, not
/// `sessionId`); `includeDetails` gates `branchChanges` + repository fields.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceWorktreeListResponse {
    #[serde(default)]
    pub worktrees: Vec<WorkspaceLinkedWorktree>,
    pub repository_branch: Option<String>,
    pub repository_cwd: Option<String>,
    pub repository_mapped_cwd: Option<String>,
    pub repository_root: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceWorktreeListParams {
    pub cwd: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub include_details: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledLoop {
    pub id: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub interval_seconds: u64,
    #[serde(default)]
    pub next_fire_at: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoopsListResponse {
    #[serde(default)]
    pub loops: Vec<ScheduledLoop>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopsCreateParams {
    pub session_id: String,
    pub interval: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoopsCreateResponse {
    #[serde(rename = "loop")]
    pub scheduled_loop: ScheduledLoop,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopsDeleteParams {
    pub session_id: String,
    pub loop_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoopsDeleteResponse {
    #[serde(rename = "loop")]
    pub scheduled_loop: ScheduledLoop,
}
