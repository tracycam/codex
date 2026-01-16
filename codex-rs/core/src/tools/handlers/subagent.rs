//! Subagent tool handler for spawning controlled recursive agents.
//!
//! This module implements the subagent TASK functionality, allowing agents to
//! spawn child agents with controlled depth and optional background execution.

use crate::agent::AgentStatus;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::config::Config;
use crate::error::CodexErr;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use async_trait::async_trait;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus as ProtocolAgentStatus;
use codex_protocol::protocol::SubagentSpawnBeginEvent;
use codex_protocol::protocol::SubagentSpawnEndEvent;
use codex_protocol::protocol::SubagentTaskCompleteEvent;
use codex_protocol::subagent::SubagentDepthContext;
use codex_protocol::subagent::SubagentOutputType;
use codex_protocol::subagent::SubagentResult;
use codex_protocol::subagent::SubagentResultStatus;
use codex_protocol::subagent::SubagentRole;
use codex_protocol::subagent::MAX_SUBAGENT_DEPTH;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Default timeout for waiting on subagent completion (5 minutes).
pub const DEFAULT_SUBAGENT_TIMEOUT_MS: u64 = 300_000;
/// Maximum timeout for subagent tasks (30 minutes).
pub const MAX_SUBAGENT_TIMEOUT_MS: u64 = 1_800_000;

pub struct SubagentHandler;

/// Arguments for the spawn_subagent tool.
#[derive(Debug, Deserialize)]
struct SpawnSubagentArgs {
    /// Role of the subagent (general, review, analyse, plan).
    #[serde(default)]
    role: SubagentRole,

    /// Task overview (one-line context).
    overview: String,

    /// Specific, verifiable goal for the subagent.
    goal: String,

    /// Requirements and constraints.
    #[serde(default)]
    requirements: Vec<String>,

    /// Expected output type.
    #[serde(default)]
    output_type: SubagentOutputType,

    /// Context file paths.
    #[serde(default)]
    context_paths: Vec<PathBuf>,

    /// Additional context or instructions.
    additional_context: Option<String>,

    /// Whether to run in background (non-blocking).
    #[serde(default)]
    background: bool,

    /// Timeout in milliseconds (for synchronous execution).
    timeout_ms: Option<u64>,

    /// Session ID for multi-round fixes.
    session_id: Option<String>,

    /// Report path for review role.
    report_path: Option<PathBuf>,

    /// Plan directory for plan role.
    plan_dir: Option<PathBuf>,
}

/// Result returned to the model for spawn_subagent.
#[derive(Debug, Serialize)]
struct SpawnSubagentResult {
    /// Thread ID of the spawned subagent.
    agent_id: String,
    /// Whether the task is running in background.
    background: bool,
    /// Status of the subagent.
    status: String,
    /// Result if completed synchronously.
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<SubagentResultOutput>,
}

/// Output portion of a subagent result.
#[derive(Debug, Serialize)]
struct SubagentResultOutput {
    status: String,
    output: Option<String>,
    round: u32,
}

impl From<SubagentResult> for SubagentResultOutput {
    fn from(r: SubagentResult) -> Self {
        Self {
            status: r.status.to_string(),
            output: r.output,
            round: r.round,
        }
    }
}

/// Arguments for get_subagent_status tool.
#[derive(Debug, Deserialize)]
struct GetSubagentStatusArgs {
    /// Thread ID of the subagent.
    id: String,
}

/// Result for get_subagent_status tool.
#[derive(Debug, Serialize)]
struct GetSubagentStatusResult {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<SubagentResultOutput>,
}

/// Arguments for wait_subagent tool.
#[derive(Debug, Deserialize)]
struct WaitSubagentArgs {
    /// Thread ID of the subagent.
    id: String,
    /// Timeout in milliseconds.
    timeout_ms: Option<u64>,
}

/// Result for wait_subagent tool.
#[derive(Debug, Serialize)]
struct WaitSubagentResult {
    status: String,
    timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<SubagentResultOutput>,
}

#[async_trait]
impl ToolHandler for SubagentHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tool_name,
            payload,
            call_id,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "subagent handler received unsupported payload".to_string(),
                ));
            }
        };

        match tool_name.as_str() {
            "spawn_subagent" => spawn_subagent(session, turn, call_id, arguments).await,
            "get_subagent_status" => get_subagent_status(session, arguments).await,
            "wait_subagent" => wait_subagent(session, turn, call_id, arguments).await,
            "close_subagent" => close_subagent(session, turn, call_id, arguments).await,
            other => Err(FunctionCallError::RespondToModel(format!(
                "unsupported subagent tool: {other}"
            ))),
        }
    }
}

/// Spawn a new subagent with the given configuration.
async fn spawn_subagent(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    call_id: String,
    arguments: String,
) -> Result<ToolOutput, FunctionCallError> {
    let args: SpawnSubagentArgs = parse_arguments(&arguments)?;

    // Get current depth context from turn or default to depth 0.
    let parent_depth = turn.subagent_depth.unwrap_or_default();

    // Check if spawning is allowed at current depth.
    let child_depth = parent_depth.child_context().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "Cannot spawn subagent: maximum depth {} reached. Current depth: {}/{}",
            MAX_SUBAGENT_DEPTH, parent_depth.current, parent_depth.max
        ))
    })?;

    // Check if the parent role can spawn the requested child role.
    let parent_role = turn.subagent_role.unwrap_or(SubagentRole::General);
    if !parent_role.can_spawn(args.role, parent_depth.current) {
        return Err(FunctionCallError::RespondToModel(format!(
            "Role {:?} cannot spawn {:?} at depth {}. Allowed roles: {:?}",
            parent_role,
            args.role,
            parent_depth.current,
            parent_role.allowed_child_roles(parent_depth.current)
        )));
    }

    // Validate task overview and goal.
    if args.overview.trim().is_empty() || args.goal.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "Task overview and goal are required and cannot be empty".to_string(),
        ));
    }

    // Build the prompt for the subagent.
    let prompt = build_subagent_prompt(&args, &child_depth);

    // Emit spawn begin event.
    session
        .send_event(
            &turn,
            SubagentSpawnBeginEvent {
                call_id: call_id.clone(),
                parent_thread_id: session.conversation_id,
                role: args.role,
                depth: child_depth,
                background: args.background,
                overview: args.overview.clone(),
            }
            .into(),
        )
        .await;

    // Build config for the subagent.
    let config = build_subagent_config(&turn, &args, child_depth)?;

    // Spawn the subagent.
    let spawn_result = session
        .services
        .agent_control
        .spawn_agent(config, prompt)
        .await;

    match spawn_result {
        Ok(subagent_id) => {
            if args.background {
                // Background mode: return immediately with agent ID.
                let status = session
                    .services
                    .agent_control
                    .get_status(subagent_id)
                    .await;

                // Set up background callback for completion notification.
                spawn_background_watcher(
                    session.clone(),
                    turn.clone(),
                    call_id.clone(),
                    subagent_id,
                    args.role,
                    child_depth.current,
                );

                session
                    .send_event(
                        &turn,
                        SubagentSpawnEndEvent {
                            call_id,
                            parent_thread_id: session.conversation_id,
                            subagent_thread_id: Some(subagent_id),
                            role: args.role,
                            depth: child_depth.current,
                            background: true,
                            status: status.clone(),
                            result: None,
                        }
                        .into(),
                    )
                    .await;

                let content = serde_json::to_string(&SpawnSubagentResult {
                    agent_id: subagent_id.to_string(),
                    background: true,
                    status: format!("{:?}", status),
                    result: None,
                })
                .map_err(|e| FunctionCallError::Fatal(format!("Failed to serialize result: {e}")))?;

                Ok(ToolOutput::Function {
                    content,
                    success: Some(true),
                    content_items: None,
                })
            } else {
                // Synchronous mode: wait for completion.
                let timeout_ms = args
                    .timeout_ms
                    .unwrap_or(DEFAULT_SUBAGENT_TIMEOUT_MS)
                    .min(MAX_SUBAGENT_TIMEOUT_MS);

                let result =
                    wait_for_subagent_completion(&session, subagent_id, timeout_ms, args.role, child_depth.current)
                        .await;

                let status = session
                    .services
                    .agent_control
                    .get_status(subagent_id)
                    .await;

                session
                    .send_event(
                        &turn,
                        SubagentSpawnEndEvent {
                            call_id,
                            parent_thread_id: session.conversation_id,
                            subagent_thread_id: Some(subagent_id),
                            role: args.role,
                            depth: child_depth.current,
                            background: false,
                            status: status.clone(),
                            result: Some(result.clone()),
                        }
                        .into(),
                    )
                    .await;

                // Shutdown the subagent after getting result.
                let _ = session
                    .services
                    .agent_control
                    .shutdown_agent(subagent_id)
                    .await;

                let success = result.is_success();
                let content = serde_json::to_string(&SpawnSubagentResult {
                    agent_id: subagent_id.to_string(),
                    background: false,
                    status: format!("{:?}", status),
                    result: Some(result.into()),
                })
                .map_err(|e| FunctionCallError::Fatal(format!("Failed to serialize result: {e}")))?;

                Ok(ToolOutput::Function {
                    content,
                    success: Some(success),
                    content_items: None,
                })
            }
        }
        Err(err) => {
            session
                .send_event(
                    &turn,
                    SubagentSpawnEndEvent {
                        call_id,
                        parent_thread_id: session.conversation_id,
                        subagent_thread_id: None,
                        role: args.role,
                        depth: child_depth.current,
                        background: args.background,
                        status: ProtocolAgentStatus::NotFound,
                        result: None,
                    }
                    .into(),
                )
                .await;

            Err(subagent_spawn_error(err))
        }
    }
}

/// Get the status of a subagent.
async fn get_subagent_status(
    session: Arc<Session>,
    arguments: String,
) -> Result<ToolOutput, FunctionCallError> {
    let args: GetSubagentStatusArgs = parse_arguments(&arguments)?;
    let agent_id = parse_agent_id(&args.id)?;

    let status = session.services.agent_control.get_status(agent_id).await;

    let result = if let AgentStatus::Completed(output) = &status {
        Some(SubagentResultOutput {
            status: "success".to_string(),
            output: output.clone(),
            round: 1,
        })
    } else {
        None
    };

    let content = serde_json::to_string(&GetSubagentStatusResult {
        status: format!("{:?}", status),
        result,
    })
    .map_err(|e| FunctionCallError::Fatal(format!("Failed to serialize result: {e}")))?;

    Ok(ToolOutput::Function {
        content,
        success: Some(true),
        content_items: None,
    })
}

/// Wait for a subagent to complete.
async fn wait_subagent(
    session: Arc<Session>,
    _turn: Arc<TurnContext>,
    _call_id: String,
    arguments: String,
) -> Result<ToolOutput, FunctionCallError> {
    let args: WaitSubagentArgs = parse_arguments(&arguments)?;
    let agent_id = parse_agent_id(&args.id)?;

    let timeout_ms = args
        .timeout_ms
        .unwrap_or(DEFAULT_SUBAGENT_TIMEOUT_MS)
        .min(MAX_SUBAGENT_TIMEOUT_MS);

    // Get role and depth from the subagent's turn context if available.
    let role = SubagentRole::General;
    let depth = 1u8;

    let result = wait_for_subagent_completion(&session, agent_id, timeout_ms, role, depth).await;
    let timed_out = matches!(result.status, SubagentResultStatus::TimedOut);
    let success = result.is_success();

    let content = serde_json::to_string(&WaitSubagentResult {
        status: result.status.to_string(),
        timed_out,
        result: Some(result.into()),
    })
    .map_err(|e| FunctionCallError::Fatal(format!("Failed to serialize result: {e}")))?;

    Ok(ToolOutput::Function {
        content,
        success: Some(success && !timed_out),
        content_items: None,
    })
}

/// Close/shutdown a subagent.
async fn close_subagent(
    session: Arc<Session>,
    _turn: Arc<TurnContext>,
    _call_id: String,
    arguments: String,
) -> Result<ToolOutput, FunctionCallError> {
    // Reuse the collab close_agent logic but with subagent-specific error messages.
    let args: GetSubagentStatusArgs = parse_arguments(&arguments)?;
    let agent_id = parse_agent_id(&args.id)?;

    let status = session.services.agent_control.get_status(agent_id).await;

    if !matches!(status, AgentStatus::Shutdown) {
        session
            .services
            .agent_control
            .shutdown_agent(agent_id)
            .await
            .map_err(|e| subagent_error(agent_id, e))?;
    }

    let content = serde_json::to_string(&GetSubagentStatusResult {
        status: "shutdown".to_string(),
        result: None,
    })
    .map_err(|e| FunctionCallError::Fatal(format!("Failed to serialize result: {e}")))?;

    Ok(ToolOutput::Function {
        content,
        success: Some(true),
        content_items: None,
    })
}

/// Build the prompt string for the subagent based on the protocol template.
fn build_subagent_prompt(args: &SpawnSubagentArgs, depth: &SubagentDepthContext) -> String {
    let mut prompt = String::new();

    // Add depth and role tags.
    prompt.push_str(&format!("{}\n", depth.format_tag()));
    prompt.push_str(&format!("[ROLE] {:?}\n", args.role));
    prompt.push_str(&format!("[OVERVIEW] {}\n\n", args.overview));

    // Add task section.
    prompt.push_str("## Task\n");
    prompt.push_str(&format!("**Goal**: {}\n", args.goal));

    if !args.requirements.is_empty() {
        prompt.push_str(&format!(
            "**Requirements**: {}\n",
            args.requirements.join("; ")
        ));
    }

    prompt.push_str(&format!("**Output**: {:?}\n", args.output_type));

    if !args.context_paths.is_empty() {
        let paths: Vec<String> = args
            .context_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        prompt.push_str(&format!("**Context**: {}\n", paths.join(", ")));
    }

    // Add optional fields.
    if let Some(ref report_path) = args.report_path {
        prompt.push_str(&format!("**report_path**: {}\n", report_path.display()));
    }
    if let Some(ref plan_dir) = args.plan_dir {
        prompt.push_str(&format!("**plan_dir**: {}\n", plan_dir.display()));
    }
    if let Some(ref session_id) = args.session_id {
        prompt.push_str(&format!("**SESSION_ID**: {}\n", session_id));
    }

    if let Some(ref additional) = args.additional_context {
        prompt.push_str(&format!("\n{}\n", additional));
    }

    prompt
}

/// Build config for the subagent.
fn build_subagent_config(
    turn: &TurnContext,
    args: &SpawnSubagentArgs,
    child_depth: SubagentDepthContext,
) -> Result<Config, FunctionCallError> {
    let base_config = turn.client.config();
    let mut config = (*base_config).clone();

    config.model = Some(turn.client.get_model());
    config.model_provider = turn.client.get_provider();
    config.model_reasoning_effort = turn.client.get_reasoning_effort();
    config.model_reasoning_summary = turn.client.get_reasoning_summary();
    config.developer_instructions = turn.developer_instructions.clone();
    config.base_instructions = turn.base_instructions.clone();
    config.compact_prompt = turn.compact_prompt.clone();
    config.user_instructions = turn.user_instructions.clone();
    config.shell_environment_policy = turn.shell_environment_policy.clone();
    config.codex_linux_sandbox_exe = turn.codex_linux_sandbox_exe.clone();
    config.cwd = turn.cwd.clone();

    // Set subagent-specific context.
    config.subagent_depth = Some(child_depth);
    config.subagent_role = Some(args.role);

    // Apply role-specific sandbox policy.
    if args.role.is_read_only() {
        // Read-only roles get read-only sandbox by default.
        config
            .sandbox_policy
            .set(codex_protocol::protocol::SandboxPolicy::new_read_only_policy())
            .map_err(|e| FunctionCallError::RespondToModel(format!("Invalid sandbox policy: {e}")))?;
    } else {
        config
            .sandbox_policy
            .set(turn.sandbox_policy.clone())
            .map_err(|e| FunctionCallError::RespondToModel(format!("Invalid sandbox policy: {e}")))?;
    }

    config
        .approval_policy
        .set(turn.approval_policy)
        .map_err(|e| FunctionCallError::RespondToModel(format!("Invalid approval policy: {e}")))?;

    Ok(config)
}

/// Wait for a subagent to reach a final status.
async fn wait_for_subagent_completion(
    session: &Session,
    agent_id: ThreadId,
    timeout_ms: u64,
    role: SubagentRole,
    depth: u8,
) -> SubagentResult {
    use crate::agent::status::is_final;

    let status_rx = match session
        .services
        .agent_control
        .subscribe_status(agent_id)
        .await
    {
        Ok(rx) => rx,
        Err(_) => {
            return SubagentResult {
                agent_id,
                role,
                depth,
                status: SubagentResultStatus::Failed,
                output: Some("Failed to subscribe to subagent status".to_string()),
                round: 1,
            };
        }
    };

    let mut status_rx = status_rx;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        let status = status_rx.borrow_and_update().clone();

        if is_final(&status) {
            let (result_status, output) = match status {
                AgentStatus::Completed(msg) => (SubagentResultStatus::Success, msg),
                AgentStatus::Errored(err) => (SubagentResultStatus::Failed, Some(err)),
                AgentStatus::Shutdown => (SubagentResultStatus::Cancelled, None),
                _ => (SubagentResultStatus::Failed, None),
            };

            return SubagentResult {
                agent_id,
                role,
                depth,
                status: result_status,
                output,
                round: 1,
            };
        }

        match tokio::time::timeout_at(deadline, status_rx.changed()).await {
            Ok(Ok(())) => continue,
            Ok(Err(_)) => {
                // Channel closed.
                return SubagentResult {
                    agent_id,
                    role,
                    depth,
                    status: SubagentResultStatus::Failed,
                    output: Some("Subagent channel closed unexpectedly".to_string()),
                    round: 1,
                };
            }
            Err(_) => {
                // Timeout.
                return SubagentResult {
                    agent_id,
                    role,
                    depth,
                    status: SubagentResultStatus::TimedOut,
                    output: None,
                    round: 1,
                };
            }
        }
    }
}

/// Spawn a background task to watch for subagent completion and notify parent.
fn spawn_background_watcher(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    call_id: String,
    subagent_id: ThreadId,
    role: SubagentRole,
    depth: u8,
) {
    tokio::spawn(async move {
        // Use a long timeout for background tasks.
        let result = wait_for_subagent_completion(
            &session,
            subagent_id,
            MAX_SUBAGENT_TIMEOUT_MS,
            role,
            depth,
        )
        .await;

        // Emit completion event.
        session
            .send_event(
                &turn,
                SubagentTaskCompleteEvent {
                    call_id,
                    parent_thread_id: session.conversation_id,
                    subagent_thread_id: subagent_id,
                    result,
                }
                .into(),
            )
            .await;

        // Cleanup: shutdown the subagent.
        let _ = session
            .services
            .agent_control
            .shutdown_agent(subagent_id)
            .await;
    });
}

fn parse_agent_id(id: &str) -> Result<ThreadId, FunctionCallError> {
    ThreadId::from_string(id)
        .map_err(|e| FunctionCallError::RespondToModel(format!("Invalid agent id {id}: {e:?}")))
}

fn subagent_spawn_error(err: CodexErr) -> FunctionCallError {
    match err {
        CodexErr::UnsupportedOperation(_) => {
            FunctionCallError::RespondToModel("Subagent manager unavailable".to_string())
        }
        err => FunctionCallError::RespondToModel(format!("Subagent spawn failed: {err}")),
    }
}

fn subagent_error(agent_id: ThreadId, err: CodexErr) -> FunctionCallError {
    match err {
        CodexErr::ThreadNotFound(id) => {
            FunctionCallError::RespondToModel(format!("Subagent with id {id} not found"))
        }
        CodexErr::InternalAgentDied => {
            FunctionCallError::RespondToModel(format!("Subagent with id {agent_id} is closed"))
        }
        CodexErr::UnsupportedOperation(_) => {
            FunctionCallError::RespondToModel("Subagent manager unavailable".to_string())
        }
        err => FunctionCallError::RespondToModel(format!("Subagent operation failed: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_format_includes_depth_and_role() {
        let args = SpawnSubagentArgs {
            role: SubagentRole::Analyse,
            overview: "Analyze the codebase".to_string(),
            goal: "Identify potential issues".to_string(),
            requirements: vec!["Be thorough".to_string()],
            output_type: SubagentOutputType::Report,
            context_paths: vec![],
            additional_context: None,
            background: false,
            timeout_ms: None,
            session_id: None,
            report_path: None,
            plan_dir: None,
        };
        let depth = SubagentDepthContext::new(1);

        let prompt = build_subagent_prompt(&args, &depth);

        assert!(prompt.contains("[DEPTH] 1/3"));
        assert!(prompt.contains("[ROLE] Analyse"));
        assert!(prompt.contains("[OVERVIEW] Analyze the codebase"));
        assert!(prompt.contains("**Goal**: Identify potential issues"));
        assert!(prompt.contains("Be thorough"));
        assert!(prompt.contains("Report"));
    }
}
