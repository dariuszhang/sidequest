use std::path::Path;

use chrono::{DateTime, Local};

use crate::config::{PromptsConfig, WorkMode};
use crate::state::RemainingWorkItem;

const TASK_PROMPT_TEMPLATE: &str = include_str!("../defaults/prompts/task_template.md");

pub struct TaskPromptSpec<'a> {
    pub mode: WorkMode,
    pub title: &'a str,
    pub context: &'a str,
    pub remaining: &'a [RemainingWorkItem],
    pub prompts: &'a PromptsConfig,
    pub spendable_budget: f64,
    pub cutoff_time: DateTime<Local>,
    pub report_path: &'a Path,
}

pub fn build_task_prompt(spec: &TaskPromptSpec<'_>) -> String {
    let mode_prompt = match spec.mode {
        WorkMode::Grind => spec.prompts.actions.grind.as_str(),
        WorkMode::Quest => spec.prompts.actions.quest.as_str(),
    };

    let replacements = [
        ("{{PRIMARY_MODE}}", spec.mode.to_string()),
        ("{{PRIMARY_TASK}}", spec.title.to_string()),
        (
            "{{SAFE_BUDGET_PERCENT}}",
            format!("{:.0}%", spec.spendable_budget * 100.0),
        ),
        (
            "{{DEADLINE_LOCAL}}",
            spec.cutoff_time.format("%Y-%m-%d %H:%M").to_string(),
        ),
        ("{{PRIMARY_TASK_CONTEXT}}", spec.context.trim().to_string()),
        ("{{REMAINING_WORK_BLOCK}}", remaining_block(spec.remaining)),
        (
            "{{WORK_DELEGATION_PROMPT}}",
            spec.prompts.work_delegation.trim().to_string(),
        ),
        ("{{MODE_PROMPT}}", mode_prompt.trim().to_string()),
        ("{{REPORT_PATH}}", spec.report_path.display().to_string()),
    ];

    render_template(TASK_PROMPT_TEMPLATE, &replacements)
}

fn remaining_block(remaining: &[RemainingWorkItem]) -> String {
    if remaining.is_empty() {
        return "No additional queued work is currently known.".to_string();
    }

    remaining
        .iter()
        .take(8)
        .map(|item| format!("- [{}] {}", item.mode, item.summary))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_template(template: &str, replacements: &[(&str, String)]) -> String {
    let mut rendered = template.to_string();
    for (placeholder, value) in replacements {
        rendered = rendered.replace(placeholder, value);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use std::path::PathBuf;

    fn sample_spec(mode: WorkMode) -> TaskPromptSpec<'static> {
        static CONTEXT: &str = "Repository evidence here.";
        static TITLE: &str = "Do the task";

        let remaining = Box::leak(Box::new(vec![RemainingWorkItem {
            mode: WorkMode::Grind,
            repository: Some("/tmp/repo".to_string()),
            quest: None,
            summary: "Follow-up work".to_string(),
            next_step: Some("Take the next step".to_string()),
        }]));
        let report_path = Box::leak(Box::new(PathBuf::from(
            "/tmp/repo/.sidequest/session-report.json",
        )));
        let prompts = Box::leak(Box::new(PromptsConfig::default()));

        TaskPromptSpec {
            mode,
            title: TITLE,
            context: CONTEXT,
            remaining,
            prompts,
            spendable_budget: 0.42,
            cutoff_time: Local::now(),
            report_path,
        }
    }

    #[test]
    fn grind_prompt_includes_autonomy_and_triage_guidance() {
        let prompt = build_task_prompt(&sample_spec(WorkMode::Grind));

        assert!(prompt.contains("Operate autonomously for this run"));
        assert!(prompt.contains("Never ask the sleeping developer what to do next"));
        assert!(prompt.contains("Pick the single highest-value change"));
    }

    #[test]
    fn prompt_includes_remaining_work() {
        let prompt = build_task_prompt(&sample_spec(WorkMode::Grind));

        assert!(prompt.contains("Known remaining work after this task"));
        assert!(prompt.contains("- [grind] Follow-up work"));
    }

    #[test]
    fn quest_prompt_mentions_continuity() {
        let prompt = build_task_prompt(&sample_spec(WorkMode::Quest));

        assert!(prompt.contains("Ship the next smallest useful increment"));
        assert!(prompt.contains("Leave a crisp `next_step`"));
        assert!(prompt.contains("quest_completed"));
    }
}
