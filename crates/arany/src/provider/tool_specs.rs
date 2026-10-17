use super::PromptRole;
use crate::tools::catalog::{by_name, catalog_call_schemas, facts_of, plan_contract_property};
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

pub(crate) fn specs(catalog: &Value, role: PromptRole) -> Vec<ToolSpec> {
    let mut calls = catalog_call_schemas(catalog);
    if role == PromptRole::Researcher {
        calls.retain(|call| facts_of(call).is_some_and(|facts| facts.researcher));
    }
    let contract = (role == PromptRole::Primary).then(|| plan_contract_property(&calls));
    calls
        .into_iter()
        .map(|mut call| {
            if let Some(contract) = &contract {
                call["properties"]
                    .as_object_mut()
                    .expect("typed Tool schema")
                    .insert("plan_contract".into(), contract.clone());
                call["required"]
                    .as_array_mut()
                    .expect("typed Tool required fields")
                    .push("plan_contract".into());
            }
            spec(call, "operation")
        })
        .collect()
}

pub(crate) fn outcome_specs(outcome: &Value) -> Vec<ToolSpec> {
    outcome["properties"]["outcome"]["anyOf"]
        .as_array()
        .expect("compiled outcome branches")
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
        "finish" => {
            "Complete the current task and report its observed result. A requested edit must first have an applied mutation result, or an actual blocking failure."
        }
        "delegate" => {
            "Delegate independent read-only reasoning to children; the primary retains file operations and completes the task after their results."
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
        let primary = specs(&catalog(), PromptRole::Primary);
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
            ]
        );
        assert!(primary.iter().all(|spec| !spec.description.is_empty()
            && spec.parameters["properties"].get("operation").is_none()
            && spec.parameters["additionalProperties"] == false));
    }

    #[test]
    fn primary_tools_carry_a_required_nullable_plan_contract() {
        for spec in specs(&catalog(), PromptRole::Primary) {
            let required = spec.parameters["required"].as_array().unwrap();
            assert!(required.contains(&json!("plan_contract")), "{}", spec.name);
            let contract = &spec.parameters["properties"]["plan_contract"]["anyOf"];
            assert_eq!(contract[1], json!({"type": "null"}), "{}", spec.name);
            assert_eq!(
                contract[0]["properties"]["skills"]["items"]["required"],
                json!(["name", "resource"])
            );
        }
        let criterion = &specs(&catalog(), PromptRole::Primary)[0].parameters["properties"]["plan_contract"]
            ["anyOf"][0]["properties"]["success_criterion"]["anyOf"][0]["properties"]["call"]["anyOf"];
        let eligible: Vec<_> = criterion
            .as_array()
            .unwrap()
            .iter()
            .map(|call| call["properties"]["operation"]["enum"][0].as_str().unwrap())
            .collect();
        assert_eq!(
            eligible,
            ["skill", "mkdir", "write", "edit", "command", "mcp_call"]
        );
    }

    #[test]
    fn the_catalog_filters_tools_and_pins_names_without_changing_the_order() {
        let read_only = json!({"write": false, "commands": [], "skills": [], "mcp_servers": [], "git": "unavailable"});
        assert_eq!(
            names(&specs(&read_only, PromptRole::Primary)),
            ["arany_list", "arany_read", "arany_search"]
        );
        let primary = specs(&catalog(), PromptRole::Primary);
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
        assert!(names(&specs(&Value::Null, PromptRole::Primary)).len() <= 4);
    }

    #[test]
    fn researchers_get_only_read_only_research_tools_without_plan_contracts() {
        let researcher = specs(&catalog(), PromptRole::Researcher);
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
        let schema = json!({"properties": {"outcome": {"anyOf": [
            {"properties": {"type": {"enum": ["finish"]}, "summary": {"type": "string"}, "result": {"type": "string"}}, "required": ["type", "summary", "result"]},
        ]}}});
        let outcomes = outcome_specs(&schema);
        assert_eq!(names(&outcomes), ["arany_finish"]);
        assert!(outcomes[0].parameters["properties"].get("type").is_none());
    }
}
