//! Subagent Protocol definitions for controlled recursive agent spawning.
//!
//! This module defines the data structures for the Subagent TASK system,
//! which allows agents to spawn child agents with controlled depth and
//! optional background execution.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use strum_macros::Display;
use ts_rs::TS;

/// Maximum allowed subagent depth (0 = main agent, 3 = terminal).
pub const MAX_SUBAGENT_DEPTH: u8 = 3;

/// Subagent role determines the permissions and capabilities of a spawned agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum SubagentRole {
    /// General-purpose agent with workspace write permissions.
    /// Can spawn: general, review, analyse, plan at depth ≤ 1; only analyse at depth = 2.
    General,

    /// Review agent with read-only permissions (write only if report_path given).
    /// Can only spawn: analyse.
    Review,

    /// Analysis agent with read-only permissions.
    /// Can only spawn: analyse.
    Analyse,

    /// Planning agent with read-only permissions (write only if plan_dir given).
    /// Can only spawn: analyse.
    Plan,
}

impl Default for SubagentRole {
    fn default() -> Self {
        SubagentRole::General
    }
}

impl SubagentRole {
    /// Returns whether this role has read-only permissions by default.
    pub fn is_read_only(&self) -> bool {
        matches!(self, SubagentRole::Review | SubagentRole::Analyse | SubagentRole::Plan)
    }

    /// Returns the roles that this role can spawn at the given depth.
    pub fn allowed_child_roles(&self, depth: u8) -> Vec<SubagentRole> {
        match self {
            SubagentRole::General => {
                if depth <= 1 {
                    vec![
                        SubagentRole::General,
                        SubagentRole::Review,
                        SubagentRole::Analyse,
                        SubagentRole::Plan,
                    ]
                } else if depth == 2 {
                    vec![SubagentRole::Analyse]
                } else {
                    vec![]
                }
            }
            SubagentRole::Review | SubagentRole::Analyse | SubagentRole::Plan => {
                vec![SubagentRole::Analyse]
            }
        }
    }

    /// Returns whether this role can spawn the given child role at the given depth.
    pub fn can_spawn(&self, child_role: SubagentRole, depth: u8) -> bool {
        if depth >= MAX_SUBAGENT_DEPTH {
            return false;
        }
        self.allowed_child_roles(depth).contains(&child_role)
    }
}

/// Execution strategy based on subagent depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum SubagentStrategy {
    /// Depth 0: Orchestrate - delegate via subagents, coordinate & review.
    Orchestrate,

    /// Depth 1: Balance - simple tasks execute, complex ones use plan→general→review loop.
    Balance,

    /// Depth 2: Execute - self-complete, only analyse subagent allowed.
    Execute,

    /// Depth 3: Terminal - must complete alone, NO subagents.
    Terminal,
}

impl SubagentStrategy {
    /// Returns the strategy for a given depth.
    pub fn for_depth(depth: u8) -> Self {
        match depth {
            0 => SubagentStrategy::Orchestrate,
            1 => SubagentStrategy::Balance,
            2 => SubagentStrategy::Execute,
            _ => SubagentStrategy::Terminal,
        }
    }

    /// Returns whether subagent spawning is allowed for this strategy.
    pub fn allows_subagents(&self) -> bool {
        !matches!(self, SubagentStrategy::Terminal)
    }
}

/// Depth context for tracking subagent nesting level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
pub struct SubagentDepthContext {
    /// Current depth (0 = main agent).
    pub current: u8,
    /// Maximum allowed depth.
    pub max: u8,
}

impl Default for SubagentDepthContext {
    fn default() -> Self {
        Self {
            current: 0,
            max: MAX_SUBAGENT_DEPTH,
        }
    }
}

impl SubagentDepthContext {
    /// Creates a new depth context with default max depth.
    pub fn new(current: u8) -> Self {
        Self {
            current,
            max: MAX_SUBAGENT_DEPTH,
        }
    }

    /// Creates a new depth context with custom max depth.
    pub fn with_max(current: u8, max: u8) -> Self {
        Self { current, max }
    }

    /// Returns the strategy for the current depth.
    pub fn strategy(&self) -> SubagentStrategy {
        SubagentStrategy::for_depth(self.current)
    }

    /// Returns a new context for a child agent (incremented depth).
    pub fn child_context(&self) -> Option<Self> {
        if self.current >= self.max || self.current >= MAX_SUBAGENT_DEPTH {
            None
        } else {
            Some(Self {
                current: self.current + 1,
                max: self.max,
            })
        }
    }

    /// Returns whether spawning a child is allowed.
    pub fn can_spawn_child(&self) -> bool {
        self.child_context().is_some()
    }

    /// Format as "[DEPTH] N/M" string.
    pub fn format_tag(&self) -> String {
        format!("[DEPTH] {}/{}", self.current, self.max)
    }
}

/// Configuration for spawning a subagent.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
pub struct SubagentSpawnConfig {
    /// The role of the subagent.
    pub role: SubagentRole,

    /// Depth context (inherited from parent with incremented current).
    pub depth: SubagentDepthContext,

    /// Task description for the subagent.
    pub task: SubagentTask,

    /// Whether to run in background (non-blocking).
    #[serde(default)]
    pub background: bool,

    /// Optional session ID for multi-round fixes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,

    /// Optional report path for review role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_path: Option<std::path::PathBuf>,

    /// Optional plan directory for plan role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_dir: Option<std::path::PathBuf>,
}

/// Task specification for a subagent.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
pub struct SubagentTask {
    /// Brief overview (one-line context).
    pub overview: String,

    /// Specific, verifiable goal.
    pub goal: String,

    /// Constraints and standards.
    #[serde(default)]
    pub requirements: Vec<String>,

    /// Expected output type.
    pub output_type: SubagentOutputType,

    /// Context files and paths.
    #[serde(default)]
    pub context_paths: Vec<std::path::PathBuf>,

    /// Additional context or instructions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

/// Expected output type from a subagent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum SubagentOutputType {
    /// Analysis report.
    Report,
    /// Code diff/changes.
    Diff,
    /// Code implementation.
    Code,
    /// Execution plan.
    Plan,
    /// Generic text response.
    Text,
}

impl Default for SubagentOutputType {
    fn default() -> Self {
        SubagentOutputType::Text
    }
}

/// Result from a completed subagent task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
pub struct SubagentResult {
    /// The subagent's thread ID.
    pub agent_id: crate::ThreadId,

    /// The role that was executed.
    pub role: SubagentRole,

    /// Depth at which the subagent ran.
    pub depth: u8,

    /// Result status.
    pub status: SubagentResultStatus,

    /// The output/response from the subagent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,

    /// Round number (for multi-round sessions).
    #[serde(default = "default_round")]
    pub round: u32,
}

fn default_round() -> u32 {
    1
}

/// Status of a subagent result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Display, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum SubagentResultStatus {
    /// Task completed successfully.
    Success,
    /// Task completed with warnings.
    Partial,
    /// Task failed.
    Failed,
    /// Task timed out.
    TimedOut,
    /// Task was cancelled.
    Cancelled,
}

impl SubagentResult {
    /// Returns whether the result indicates success.
    pub fn is_success(&self) -> bool {
        matches!(self.status, SubagentResultStatus::Success)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_context_child_increments() {
        let parent = SubagentDepthContext::new(0);
        let child = parent.child_context().expect("should allow child at depth 0");
        assert_eq!(child.current, 1);
        assert_eq!(child.max, MAX_SUBAGENT_DEPTH);
    }

    #[test]
    fn depth_context_blocks_at_max() {
        let terminal = SubagentDepthContext::new(MAX_SUBAGENT_DEPTH);
        assert!(terminal.child_context().is_none());
    }

    #[test]
    fn strategy_matches_depth() {
        assert_eq!(SubagentStrategy::for_depth(0), SubagentStrategy::Orchestrate);
        assert_eq!(SubagentStrategy::for_depth(1), SubagentStrategy::Balance);
        assert_eq!(SubagentStrategy::for_depth(2), SubagentStrategy::Execute);
        assert_eq!(SubagentStrategy::for_depth(3), SubagentStrategy::Terminal);
        assert_eq!(SubagentStrategy::for_depth(4), SubagentStrategy::Terminal);
    }

    #[test]
    fn general_role_spawning_rules() {
        let general = SubagentRole::General;

        // At depth 0-1, general can spawn all roles
        assert!(general.can_spawn(SubagentRole::General, 0));
        assert!(general.can_spawn(SubagentRole::Review, 0));
        assert!(general.can_spawn(SubagentRole::Analyse, 1));
        assert!(general.can_spawn(SubagentRole::Plan, 1));

        // At depth 2, general can only spawn analyse
        assert!(!general.can_spawn(SubagentRole::General, 2));
        assert!(general.can_spawn(SubagentRole::Analyse, 2));

        // At depth 3 (max), no spawning allowed
        assert!(!general.can_spawn(SubagentRole::Analyse, 3));
    }

    #[test]
    fn review_role_can_only_spawn_analyse() {
        let review = SubagentRole::Review;
        assert!(!review.can_spawn(SubagentRole::General, 0));
        assert!(!review.can_spawn(SubagentRole::Review, 0));
        assert!(review.can_spawn(SubagentRole::Analyse, 0));
        assert!(!review.can_spawn(SubagentRole::Plan, 0));
    }

    #[test]
    fn depth_format_tag() {
        let ctx = SubagentDepthContext::with_max(1, 3);
        assert_eq!(ctx.format_tag(), "[DEPTH] 1/3");
    }
}
