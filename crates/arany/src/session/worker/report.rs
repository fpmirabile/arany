use super::MAX_EDITOR_CHANGE_REPORT_BYTES;
use crate::session::{AgentView, RunView};
use crate::tools::ToolDisposition;

const HEADER: &str = "Changed paths (untrusted; committed observations, latest after-digests):\n";
const OMITTED: &str = "Further changed paths omitted.\n";

pub(crate) fn changed_paths(run: &RunView, agent: &AgentView) -> String {
    let mut report = HEADER.to_owned();
    let mut seen = Vec::new();
    for observation in agent
        .tools
        .iter()
        .rev()
        .filter_map(|tool| tool.observation.as_ref())
    {
        if observation.disposition != ToolDisposition::Succeeded
            || !observation.attested_for(&observation.intent)
            || !crate::session::checkpoint::publication(&observation.intent)
        {
            continue;
        }
        let files = crate::session::rewind::publication_files(run, observation)
            .expect("replayed publication has validated file facts");
        for file in files {
            if seen.contains(&file.path) {
                continue;
            }
            let row = format!(
                "{}\n",
                serde_json::json!({
                    "path": file.path,
                    "after_sha256": file.expected_current.as_ref().map(|state| &state.sha256),
                })
            );
            if report.len() + row.len() + OMITTED.len() > MAX_EDITOR_CHANGE_REPORT_BYTES {
                report.push_str(OMITTED);
                return report;
            }
            seen.push(file.path);
            report.push_str(&row);
        }
    }
    if seen.is_empty() {
        String::new()
    } else {
        report
    }
}
