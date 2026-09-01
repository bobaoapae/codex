use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) fn create_grant_agent_ownership_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "agent".to_string(),
            JsonSchema::string(Some(
                "Agent id or canonical task name that will own the paths.".to_string(),
            )),
        ),
        (
            "paths".to_string(),
            JsonSchema::array(
                JsonSchema::string(Some("Absolute workspace path.".to_string())),
                Some("Paths are normalized and checked against the workspace roots.".to_string()),
            ),
        ),
        (
            "mode".to_string(),
            JsonSchema::string_enum(
                vec![json!("read"), json!("write")],
                Some("Lease mode. A write lease never bypasses sandbox or approval.".to_string()),
            ),
        ),
        (
            "ttlMs".to_string(),
            JsonSchema::integer(Some(
                "Optional lease lifetime in milliseconds; defaults to 15 minutes.".to_string(),
            )),
        ),
        (
            "environment".to_string(),
            JsonSchema::string(Some(
                "Optional environment id to bind the lease to.".to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: "grant_agent_ownership".to_string(),
        description: "Root-only: grant an agent a bounded workspace path lease. This records ownership coordination only; it does not grant mutation without a later lease check, sandbox, approval, and execpolicy admission.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["agent".to_string(), "paths".to_string(), "mode".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(ownership_grant_output_schema()),
    })
}

pub(crate) fn create_release_agent_ownership_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "leaseId".to_string(),
            JsonSchema::string(Some(
                "Opaque lease id returned by grant_agent_ownership.".to_string(),
            )),
        ),
        (
            "token".to_string(),
            JsonSchema::string(Some("Current lease fencing token.".to_string())),
        ),
        (
            "generation".to_string(),
            JsonSchema::integer(Some("Current lease fencing generation.".to_string())),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: "release_agent_ownership".to_string(),
        description: "Root-only: release one workspace ownership lease with its exact fencing token and generation.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec![
                "leaseId".to_string(),
                "token".to_string(),
                "generation".to_string(),
            ]),
            Some(false.into()),
        ),
        output_schema: Some(ownership_lease_output_schema()),
    })
}

pub(crate) fn create_override_agent_ownership_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "operationDigest".to_string(),
            JsonSchema::string(Some(
                "Exact operationDigest returned by the ownership conflict denial.".to_string(),
            )),
        ),
        (
            "paths".to_string(),
            JsonSchema::array(
                JsonSchema::string(Some("Exact normalized absolute path.".to_string())),
                Some("Must match the denied operation path set exactly.".to_string()),
            ),
        ),
        (
            "reason".to_string(),
            JsonSchema::string(Some(
                "Short reason for the explicit root override; raw content is not recorded."
                    .to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: "override_agent_ownership".to_string(),
        description: "Root-only: prepare a one-shot ownership override for an exact denied operation. This appends a redacted canonical receipt and returns an unconsumed proof; it never performs the mutation.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec![
                "operationDigest".to_string(),
                "paths".to_string(),
                "reason".to_string(),
            ]),
            Some(false.into()),
        ),
        output_schema: Some(prepared_override_output_schema()),
    })
}

fn ownership_lease_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "leaseId": {"type": "string"},
            "token": {"type": "string"},
            "rootRunId": {"type": "string"},
            "ownerRunId": {"type": "string"},
            "displayPath": {"type": "string"},
            "comparisonKey": {"type": "string"},
            "mode": {"type": "string", "enum": ["read", "write"]},
            "generation": {"type": "integer"},
            "expiresAtMs": {"type": ["integer", "null"]}
        },
        "required": ["leaseId", "token", "rootRunId", "ownerRunId", "displayPath", "comparisonKey", "mode", "generation", "expiresAtMs"],
        "additionalProperties": false
    })
}

fn ownership_grant_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "leases": {"type": "array", "items": ownership_lease_output_schema()}
        },
        "required": ["leases"],
        "additionalProperties": false
    })
}

fn prepared_override_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "overrideId": {"type": "string"},
            "token": {"type": "string"},
            "generation": {"type": "integer"},
            "operationDigest": {"type": "string"},
            "paths": {"type": "array", "items": {
                "type": "object",
                "properties": {
                    "display": {"type": "string"},
                    "comparisonKey": {"type": "string"}
                },
                "required": ["display", "comparisonKey"],
                "additionalProperties": false
            }},
            "conflictOwnerRunIds": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["overrideId", "token", "generation", "operationDigest", "paths", "conflictOwnerRunIds"],
        "additionalProperties": false
    })
}
