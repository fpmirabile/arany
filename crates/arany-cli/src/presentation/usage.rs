use super::escape_terminal;
use arany::{SessionView, TokenCount, UsageGroup};
use std::collections::BTreeMap;
use std::fmt::Write;

pub(crate) const MAX_USAGE_PAGE_BYTES: usize = 8 * 1024;
const MAX_USAGE_GROUPS: usize = 16;

pub(crate) fn grouped_count(value: u64) -> String {
    let digits = value.to_string();
    let mut result = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(',');
        }
        result.push(digit);
    }
    result
}

fn count(value: TokenCount, pending: bool) -> String {
    match value.known {
        None => "unknown (not reported)".into(),
        Some(known) if value.unreported > 0 || pending => {
            format!("{} known; some calls unreported", grouped_count(known))
        }
        Some(known) => grouped_count(known),
    }
}

fn merge(target: &mut TokenCount, value: TokenCount) {
    if let Some(known) = value.known {
        target.known = Some(target.known.unwrap_or(0).saturating_add(known));
    }
    target.unreported = target.unreported.saturating_add(value.unreported);
}

pub fn usage_pages(
    view: &SessionView,
    status: bool,
    starting_sequence: Option<u64>,
) -> Vec<String> {
    let mut header = String::from("Usage: Provider-reported tokens\n");
    if status {
        if view.created_sequence == 0 {
            header.push_str("New conversation · not saved · 0 Runs\n");
        } else {
            let _ = writeln!(header, "Session {} · {} Runs", view.id, view.runs.len());
        }
        if let Some(run) = view
            .runs
            .last()
            .filter(|run| starting_sequence.is_none_or(|sequence| run.accepted_sequence > sequence))
        {
            let _ = writeln!(
                header,
                "Task: {} · {} agent{} · details: /agents",
                run.status,
                run.agents.len(),
                if run.agents.len() == 1 { "" } else { "s" }
            );
            if let Some(context) = run
                .config
                .as_ref()
                .and_then(|config| config.context_usage.as_ref())
                .filter(|_| run.status == arany::RunStatus::Active)
            {
                let _ = writeln!(
                    header,
                    "Request: {}% of local request budget; not model tokens",
                    context.utilization_percent()
                );
                header.push_str("Includes chat, instructions, files and images\n");
            }
        }
    }
    if status
        && starting_sequence.is_some_and(|sequence| {
            view.runs
                .last()
                .is_none_or(|run| run.accepted_sequence <= sequence)
        })
    {
        header.push_str("Preparing message · Ctrl+C cancels\n");
    }
    let _ = writeln!(header, "Committed through Event {}", view.last_sequence);
    header.push_str(
        "Cache reads/writes are included in input\nOutput includes reported reasoning tokens\n",
    );
    let pending = view
        .usage
        .pending
        .iter()
        .any(|(session, _, _)| *session == view.id);
    if pending {
        header.push_str("In-flight or interrupted usage pending; totals incomplete\n");
    }
    if view.defaults.provider.as_deref() == Some("chatgpt")
        && !view
            .usage
            .groups
            .iter()
            .any(|group| group.provider == "chatgpt")
    {
        header.push_str("ChatGPT weekly: unknown (not reported)\n");
    }
    let latest = view.runs.last().map(|run| run.id);
    let mut sections = Vec::new();
    for (scope, inherited, latest_only) in [
        ("Current/latest Run", false, true),
        ("Session totals", false, false),
        (
            "Inherited history (excluded from Session totals)",
            true,
            false,
        ),
    ] {
        let mut groups: BTreeMap<(&str, &str), [TokenCount; 4]> = BTreeMap::new();
        for group in &view.usage.groups {
            if (group.session_id != view.id) != inherited
                || latest_only && Some(group.run_id) != latest
            {
                continue;
            }
            let total = groups.entry((&group.provider, &group.model)).or_default();
            for (target, value) in total.iter_mut().zip(counters(group)) {
                merge(target, value);
            }
        }
        let scope_pending = view.usage.pending.iter().any(|(session, run, _)| {
            (*session != view.id) == inherited && (!latest_only || Some(*run) == latest)
        });
        if groups.is_empty() && (!inherited || scope_pending) {
            header.push_str(&format!(
                "\n{scope}: {}\n",
                if scope_pending {
                    "usage pending; totals incomplete"
                } else {
                    "no calls yet"
                }
            ));
        }
        for ((provider, model), values) in groups {
            let mut text = format!(
                "\n{scope}\nProvider: {}\nModel: {}\n",
                escape_terminal(provider),
                escape_terminal(model)
            );
            for (label, value) in [
                "Input",
                "Output",
                "Cache read (included in input)",
                "Cache write (included in input)",
            ]
            .into_iter()
            .zip(values)
            {
                let _ = writeln!(text, "{label}: {}", count(value, scope_pending));
            }
            text.push_str(if provider == "chatgpt" {
                "ChatGPT weekly: unknown (not reported)\n"
            } else {
                "API rate limits: unknown (not reported)\n"
            });
            sections.push(text);
        }
    }
    if sections.is_empty() {
        header.push_str("Limits: unknown (not reported)\n");
    }
    let total = sections.len();
    let mut pages = Vec::new();
    let mut page = header.clone();
    let mut start = 0;
    let mut end = 0;
    for section in sections {
        if end - start >= MAX_USAGE_GROUPS || page.len() + section.len() + 96 > MAX_USAGE_PAGE_BYTES
        {
            let _ = writeln!(page, "Groups {}-{end} of {total}", start + 1);
            pages.push(page);
            page = header.clone();
            start = end;
        }
        page.push_str(&section);
        end += 1;
    }
    if !pages.is_empty() {
        let _ = writeln!(page, "Groups {}-{end} of {total}", start + 1);
    }
    pages.push(page);
    pages
}

fn counters(group: &UsageGroup) -> [TokenCount; 4] {
    [
        group.input,
        group.output,
        group.cache_read,
        group.cache_creation,
    ]
}
