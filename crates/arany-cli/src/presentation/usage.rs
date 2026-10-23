use super::escape_terminal;
use arany::{ProviderKind, SessionView, TokenCount, UsageGroup};
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
    usage_pages_at(view, status, starting_sequence, None)
}

pub fn usage_pages_at(
    view: &SessionView,
    status: bool,
    starting_sequence: Option<u64>,
    now: Option<i64>,
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
    if view
        .defaults
        .provider
        .as_deref()
        .and_then(ProviderKind::parse)
        == Some(ProviderKind::ChatGpt)
        && !view
            .usage
            .groups
            .iter()
            .any(|group| ProviderKind::parse(&group.provider) == Some(ProviderKind::ChatGpt))
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
            text.push_str(
                if ProviderKind::parse(provider) == Some(ProviderKind::ChatGpt) {
                    if view.usage.plan_limits.is_empty() {
                        "ChatGPT weekly: unknown (not reported)\n"
                    } else {
                        "Plan windows: see account-level observations below\n"
                    }
                } else {
                    "API rate limits: unknown (not reported)\n"
                },
            );
            sections.push(text);
        }
    }
    for (index, group) in view.usage.plan_limits.iter().enumerate() {
        let run_number = view
            .runs
            .iter()
            .position(|run| run.id == group.run_id)
            .map_or(0, |index| index + 1);
        let historical = group.session_id != view.id
            || view
                .runs
                .last()
                .is_none_or(|run| run.id != group.run_id || run.status != arany::RunStatus::Active);
        sections.push(plan_section(group, run_number, index + 1, historical, now));
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

fn plan_section(
    group: &arany::PlanUsage,
    run_number: usize,
    selection: usize,
    historical: bool,
    now: Option<i64>,
) -> String {
    let mut text = format!(
        "\nChatGPT plan · Run {run_number} · route {selection}{}\nProvider: {}\nModel: {}\nAccount-level observation; not this Session's share\n",
        if historical { " · historical" } else { "" },
        escape_terminal(&group.provider),
        escape_terminal(&group.model)
    );
    let windows = [group.primary.as_ref(), group.secondary.as_ref()];
    if !windows
        .iter()
        .flatten()
        .any(|window| window.window_minutes == Some(10_080))
    {
        text.push_str("ChatGPT weekly: unknown (not reported)\n");
    }
    for window in windows.into_iter().flatten() {
        window_lines(&mut text, window, now);
    }
    if group.latest_unreported {
        text.push_str("Latest call did not report limits\n");
    }
    if let Some(failure) = group.latest_unavailable {
        let reason = match failure.reason {
            arany::LimitUnavailable::Invalid => "invalid optional metadata",
            arany::LimitUnavailable::Duplicate => "duplicate optional metadata",
            arany::LimitUnavailable::Oversized => "optional metadata exceeded bounds",
        };
        let _ = writeln!(
            text,
            "Limits unavailable: {reason} · last reported {}",
            utc_timestamp(failure.observed_at)
        );
    }
    text
}

fn window_lines(text: &mut String, window: &arany::PlanWindow, now: Option<i64>) {
    let label = match window.window_minutes {
        Some(300) => "5-hour".into(),
        Some(10_080) => "weekly".into(),
        Some(minutes) => format!("{minutes}-minute window"),
        None => "window duration unknown".into(),
    };
    let percent = window.used_percent.map_or_else(
        || "unknown (not reported)".into(),
        |p| format!("{}% used", (f64::from(p) * 10.0).round() / 10.0),
    );
    let _ = writeln!(text, "ChatGPT {label}: {percent}");
    let source = match window.source {
        arany::LimitSource::Headers => "headers",
        arany::LimitSource::Event => "event",
    };
    let _ = writeln!(
        text,
        "Last reported: {} · {source}",
        utc_timestamp(window.observed_at)
    );
    let age = now
        .and_then(|now| now.checked_sub(window.observed_at))
        .filter(|age| *age >= 0);
    if let Some(age) = age {
        let _ = writeln!(text, "Observation age: {}", format_duration(age));
    } else {
        text.push_str("Observation age unknown\n");
    }
    match window.reset_at {
        Some(reset) => {
            let _ = writeln!(text, "Reset: {}", utc_timestamp(reset));
            if age.is_some()
                && let Some(remaining) = now
                    .and_then(|now| reset.checked_sub(now))
                    .filter(|seconds| *seconds > 0)
            {
                let _ = writeln!(text, "Resets in {}", format_duration(remaining));
            }
        }
        None => text.push_str("Reset unknown\n"),
    }
    if now
        .zip(window.reset_at)
        .is_some_and(|(now, reset)| now >= reset)
    {
        text.push_str("Stale; reset time passed, current usage unknown\n");
    } else if age.is_some_and(|age| age > 15 * 60) {
        text.push_str("Stale; last reported more than 15 minutes ago, current usage unknown\n");
    }
}

fn format_duration(mut seconds: i64) -> String {
    let mut parts = Vec::with_capacity(2);
    for (size, unit) in [(86_400, "d"), (3600, "h"), (60, "min"), (1, "s")] {
        let count = seconds / size;
        seconds %= size;
        if count > 0 {
            parts.push(if unit == "d" {
                format!("{count} d")
            } else {
                super::format_duration(
                    u64::try_from(count * size * 1000).expect("bounded sub-day duration"),
                )
            });
            if parts.len() == 2 {
                break;
            }
        }
    }
    if parts.is_empty() {
        "0 s".into()
    } else {
        parts.join(" ")
    }
}

fn utc_timestamp(seconds: i64) -> String {
    if !(0..=arany::MAX_LIMIT_TIMESTAMP).contains(&seconds) {
        return "unknown".into();
    }
    let (year, month, day) =
        arany::utc_date(u64::try_from(seconds).expect("nonnegative timestamp"));
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reopened_plan_report_uses_only_committed_historical_observations() {
        use arany::test_support::{engine::run_to_end, routing::*, scenario::ScriptedProvider};
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let mut provider = ScriptedProvider::new(vec![finish("done")])
            .with_chatgpt_account(uuid::Uuid::now_v7(), [1; 32]);
        let mut sample = plan_sample(1000);
        sample.primary.as_mut().unwrap().reset_at = Some(9100);
        provider
            .rate_limits
            .get_mut()
            .unwrap()
            .push_back(Some(sample));
        let provider = route(provider);
        let engine = open(&state);
        let outcome = run_to_end(&engine, &provider, input(&workspace))
            .await
            .unwrap();
        engine.close().await.unwrap();
        let (_, view) = reopen(&state, outcome.session_id).await;
        let before = view.clone();
        let expected = "\nChatGPT plan · Run 1 · route 1 · historical\nProvider: chatgpt\nModel: model-a\nAccount-level observation; not this Session's share\nChatGPT 5-hour: 25.5% used\nLast reported: 1970-01-01T00:16:40Z · headers\nObservation age: 0 s\nReset: 1970-01-01T02:31:40Z\nResets in 2 h 15 min\nChatGPT weekly: 70% used\nLast reported: 1970-01-01T00:16:40Z · headers\nObservation age: 0 s\nReset unknown\n";
        for status in [false, true] {
            let pages = usage_pages_at(&view, status, None, Some(1000));
            assert_eq!(pages.len(), 1);
            assert!(pages[0].ends_with(expected), "{}", pages[0]);
        }
        assert_eq!(view, before);
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn plan_windows_keep_receipt_times_and_qualify_reset_age_and_missing_fields() {
        let mut window = arany::test_support::routing::plan_sample(1_000)
            .primary
            .unwrap();
        window.reset_at = Some(9_100);
        let mut text = String::new();
        window_lines(&mut text, &window, Some(1_000));
        assert_eq!(
            text,
            "ChatGPT 5-hour: 25.5% used\nLast reported: 1970-01-01T00:16:40Z · headers\nObservation age: 0 s\nReset: 1970-01-01T02:31:40Z\nResets in 2 h 15 min\n"
        );
        for (now, qualification) in [
            (
                Some(9_100),
                "Stale; reset time passed, current usage unknown",
            ),
            (
                Some(1_901),
                "Stale; last reported more than 15 minutes ago, current usage unknown",
            ),
            (Some(999), "Observation age unknown"),
            (None, "Observation age unknown"),
        ] {
            text.clear();
            window_lines(&mut text, &window, now);
            assert!(text.contains(qualification));
            assert!(text.contains("25.5% used"));
            assert!(text.contains("1970-01-01T00:16:40Z"));
        }
        window.used_percent = None;
        window.window_minutes = None;
        window.reset_at = None;
        text.clear();
        window_lines(&mut text, &window, None);
        assert!(text.contains("window duration unknown: unknown (not reported)"));
        assert!(text.contains("Reset unknown"));
        assert!(!text.contains("weekly"));
        assert_eq!(utc_timestamp(1_704_067_200), "2024-01-01T00:00:00Z");
        assert_eq!(
            utc_timestamp(arany::MAX_LIMIT_TIMESTAMP),
            "9999-12-31T23:59:59Z"
        );
        window.used_percent = Some(arany::UsedPercent::try_from(100.0 / 3.0).unwrap());
        window.reset_at = Some(1000 + 5400 + 150 * 3600 + 3 * 60);
        text.clear();
        window_lines(&mut text, &window, Some(1000 + 5400));
        assert!(text.contains("33.3% used"));
        assert!(text.contains("Observation age: 1 h 30 min\n"));
        assert!(text.contains("Resets in 6 d 6 h\n"));
        for (seconds, expected) in [
            (0, "0 s"),
            (59, "59 s"),
            (60, "1 min"),
            (61, "1 min 1 s"),
            (3600, "1 h"),
            (3661, "1 h 1 min"),
            (86400, "1 d"),
            (86461, "1 d 1 min"),
        ] {
            assert_eq!(format_duration(seconds), expected);
        }
        for (percent, expected) in [
            (0.0, "0% used"),
            (12.34, "12.3% used"),
            (99.99, "100% used"),
        ] {
            window.used_percent = Some(arany::UsedPercent::try_from(percent).unwrap());
            text.clear();
            window_lines(&mut text, &window, None);
            assert!(text.contains(expected));
        }
    }
}
