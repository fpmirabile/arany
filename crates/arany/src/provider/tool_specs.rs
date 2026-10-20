use super::PromptRole;
use crate::tools::catalog::{by_name, catalog_call_schemas, facts_of, plan_schema};
use serde_json::Value;

pub(crate) const NAME_PREFIX: &str = "arany_";

#[derive(Clone, Debug)]
pub(crate) struct ToolSpec {
    pub(crate) name: String,
    pub(crate) description: &'static str,
    pub(crate) parameters: Value,
}

pub(crate) fn name_of(operation: &str) -> String {
    format!("{NAME_PREFIX}{operation}")
}

pub(crate) fn specs(
    catalog: &Value,
    role: PromptRole,
    pinned: &crate::session::GoalBudget,
) -> Vec<ToolSpec> {
    let mut calls = catalog_call_schemas(catalog);
    if role == PromptRole::Researcher {
        calls.retain(|call| facts_of(call).is_some_and(|facts| facts.researcher));
    } else if role == PromptRole::Editor {
        calls.retain(|call| {
            facts_of(call).is_some_and(|facts| facts.family == crate::tools::catalog::Family::File)
        });
    }
    let plan = (role == PromptRole::Primary).then(|| ToolSpec {
        name: name_of("plan"),
        description: "Optionally record a bounded Goal plan and exact success criterion. A recorded plan only narrows scope and cannot be replaced.",
        parameters: plan_schema(&calls, pinned),
    });
    let mut specs: Vec<_> = calls
        .into_iter()
        .map(|call| spec(call, "operation"))
        .collect();
    specs.extend(plan);
    specs
}

pub(crate) fn outcome_specs(outcome: &Value) -> Vec<ToolSpec> {
    outcome["properties"]["outcome"]["anyOf"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|branch| spec(branch.clone(), "type"))
        .collect()
}

fn spec(mut parameters: Value, discriminator: &str) -> ToolSpec {
    let operation = parameters["properties"][discriminator]["enum"][0]
        .as_str()
        .expect("compiled function name")
        .to_owned();
    parameters["properties"]
        .as_object_mut()
        .expect("properties")
        .remove(discriminator);
    parameters["required"]
        .as_array_mut()
        .expect("required")
        .retain(|field| field != discriminator);
    let description = match operation.as_str() {
        "delegate" => {
            "Delegate a bounded team of researchers and offered editor assignments in the next settled wave. The primary integrates private edits and verifies the criterion before completing."
        }
        _ => by_name(&operation).expect("compiled operation").description,
    };
    ToolSpec {
        name: name_of(&operation),
        description,
        parameters,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog() -> Value {
        json!({
            "write": true,
            "commands": [{"name": "cargo"}],
            "skills": [{"name": "guide"}],
            "mcp_servers": [{"name": "local", "tools": ["probe"]}],
            "git": {"operations": ["git_status", "git_diff", "git_log"]},
            "tool_calls_remaining": 2
        })
    }

    fn names(specs: &[ToolSpec]) -> Vec<&str> {
        specs.iter().map(|spec| spec.name.as_str()).collect()
    }

    #[test]
    fn tool_specs_cover_every_catalog_operation() {
        let primary = specs(&catalog(), PromptRole::Primary, &Default::default());
        assert_eq!(
            names(&primary),
            [
                "arany_list",
                "arany_read",
                "arany_search",
                "arany_git_status",
                "arany_git_diff",
                "arany_git_log",
                "arany_skill",
                "arany_mcp_list",
                "arany_mkdir",
                "arany_write",
                "arany_edit",
                "arany_command",
                "arany_mcp_call",
                "arany_command_unguarded",
                "arany_plan"
            ]
        );
        assert!(primary.iter().all(|spec| !spec.description.is_empty()
            && spec.parameters["properties"].get("operation").is_none()
            && spec.parameters["additionalProperties"] == false));
    }

    #[test]
    fn only_the_optional_plan_function_carries_the_contract() {
        let primary = specs(&catalog(), PromptRole::Primary, &Default::default());
        for spec in &primary {
            assert!(spec.parameters["properties"].get("plan_contract").is_none());
        }
        let plan = primary.iter().find(|s| s.name == "arany_plan").unwrap();
        assert_eq!(
            plan.parameters["properties"]["skills"]["items"]["required"],
            json!(["name", "resource"])
        );
        assert!(
            jsonschema::validator_for(&plan.parameters)
                .unwrap()
                .is_valid(
                    &serde_json::to_value(crate::provider::test_support::goal_plan_contract())
                        .unwrap()
                )
        );
    }

    #[test]
    fn the_catalog_filters_tools_and_pins_names_without_changing_the_order() {
        let read_only = json!({"write": false, "commands": [], "skills": [], "mcp_servers": [], "git": "unavailable"});
        assert_eq!(
            names(&specs(&read_only, PromptRole::Primary, &Default::default())),
            ["arany_list", "arany_read", "arany_search", "arany_plan"]
        );
        let primary = specs(&catalog(), PromptRole::Primary, &Default::default());
        let command = primary
            .iter()
            .find(|spec| spec.name == "arany_command")
            .unwrap();
        assert_eq!(
            command.parameters["properties"]["program"]["enum"],
            json!(["cargo"])
        );
        let call = |operation: &str| {
            primary
                .iter()
                .find(|spec| spec.name == format!("arany_{operation}"))
                .unwrap()
        };
        assert_eq!(
            call("skill").parameters["properties"]["name"]["enum"],
            json!(["guide"])
        );
        assert_eq!(
            call("mcp_list").parameters["properties"]["server"]["enum"],
            json!(["local"])
        );
        assert!(
            names(&specs(
                &Value::Null,
                PromptRole::Primary,
                &Default::default()
            ))
            .len()
                <= 4
        );
    }

    #[test]
    fn researchers_get_only_read_only_research_tools_without_plan_contracts() {
        let researcher = specs(&catalog(), PromptRole::Researcher, &Default::default());
        assert_eq!(
            names(&researcher),
            [
                "arany_list",
                "arany_read",
                "arany_search",
                "arany_skill",
                "arany_mcp_list"
            ]
        );
        assert!(
            researcher
                .iter()
                .all(|spec| spec.parameters["properties"].get("plan_contract").is_none())
        );
    }

    #[test]
    fn outcome_functions_follow_the_restricted_schema() {
        let mut request = super::super::test_support::primary_request("model");
        request.collaboration = crate::session::CollaborationPolicy::Auto {
            max_active_children: 3,
        };
        let schema = super::super::run_outcome_schema(&request);
        let outcomes = outcome_specs(&schema);
        assert_eq!(names(&outcomes), ["arany_delegate"]);
        assert!(outcomes[0].parameters["properties"].get("type").is_none());
    }
}
