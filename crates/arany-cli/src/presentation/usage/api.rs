use super::{escape_terminal, grouped_count, observation_lines, unavailable_limits_line};
use arany::{ApiLimitDimension, ApiUsage, ProviderRole, SessionView};
use std::fmt::Write;

pub(super) fn reported(
    view: &SessionView,
    provider: &str,
    model: &str,
    (inherited, latest_only): (bool, bool),
) -> bool {
    view.usage.api_limits.iter().any(|group| {
        group.identity.profile == provider
            && group.identity.model == model
            && (group.session_id != view.id) == inherited
            && (!latest_only || view.runs.last().is_some_and(|run| run.id == group.run_id))
    })
}

pub(super) fn sections(view: &SessionView, now: Option<i64>) -> Vec<String> {
    view.usage
        .api_limits
        .iter()
        .enumerate()
        .map(|(index, group)| {
            let run_number = view
                .runs
                .iter()
                .position(|run| run.id == group.run_id)
                .map_or(0, |index| index + 1);
            let qualifier =
                if group.session_id != view.id {
                    " · inherited history · historical"
                } else if view.runs.last().is_none_or(|run| {
                    run.id != group.run_id || run.status != arany::RunStatus::Active
                }) {
                    " · historical"
                } else {
                    ""
                };
            section(group, (run_number, index + 1), qualifier, now)
        })
        .collect()
}

fn section(
    group: &ApiUsage,
    (run_number, selection): (usize, usize),
    qualifier: &str,
    now: Option<i64>,
) -> String {
    let role = match group.role {
        ProviderRole::Primary => "primary",
        ProviderRole::Child => "child",
        ProviderRole::Review => "review",
        ProviderRole::Compaction => "compaction",
    };
    let dialect = match group.dialect {
        arany::ApiLimitDialect::OpenAi => "OpenAI",
        arany::ApiLimitDialect::Anthropic => "Anthropic",
    };
    let mut text = format!(
        "\nAPI rate limits · Run {run_number} · route {selection}{qualifier}\nProvider: {}\nModel: {}\nRole: {role}\nReported header dialect: {dialect}\nProvider-reported counters; not a plan balance or this Session's share\n",
        escape_terminal(&group.identity.profile),
        escape_terminal(&group.identity.model)
    );
    for row in &group.rows {
        let count = |value: Option<u64>| {
            value
                .map(grouped_count)
                .unwrap_or_else(|| "unknown (not reported)".into())
        };
        let _ = writeln!(
            text,
            "{}\nLimit: {}\nRemaining: {}",
            label(row.dimension),
            count(row.limit),
            count(row.remaining)
        );
        observation_lines(&mut text, (row.source, row.observed_at, row.reset_at), now);
    }
    if group.rows.is_empty() {
        text.push_str("API rate limits: unknown (not reported)\n");
    }
    match &group.latest_omitted {
        Some(omitted) if omitted.is_empty() => {
            text.push_str("Latest call did not report limits\n");
        }
        Some(omitted) => {
            let labels: Vec<_> = omitted.iter().map(|dimension| label(*dimension)).collect();
            let _ = writeln!(
                text,
                "Latest call did not report limits for: {}",
                labels.join(", ")
            );
        }
        None => {}
    }
    if let Some(failure) = group.latest_unavailable {
        text.push_str(&unavailable_limits_line(failure));
    }
    text
}

fn label(dimension: ApiLimitDimension) -> &'static str {
    match dimension {
        ApiLimitDimension::Requests => "Requests",
        ApiLimitDimension::Tokens => "Tokens",
        ApiLimitDimension::ProjectTokens => "Project tokens",
        ApiLimitDimension::InputTokens => "Input tokens",
        ApiLimitDimension::OutputTokens => "Output tokens",
        ApiLimitDimension::PriorityInputTokens => "Priority input tokens",
        ApiLimitDimension::PriorityOutputTokens => "Priority output tokens",
    }
}

#[cfg(test)]
mod tests;
