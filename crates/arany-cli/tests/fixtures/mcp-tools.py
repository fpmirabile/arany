import json
import sys

mode = sys.argv[1] if len(sys.argv) == 2 else "valid"


def send(identifier, result):
    print(json.dumps({"jsonrpc": "2.0", "id": identifier, "result": result}), flush=True)


initialize = json.loads(sys.stdin.readline())
assert initialize["method"] == "initialize"
assert initialize["params"]["protocolVersion"] == "2025-11-25"
assert initialize["params"]["capabilities"] == {}
if mode == "oversized_frame":
    print("x" * (128 * 1024 + 1), flush=True)
    sys.exit(0)
if mode == "notification_flood":
    for _ in range(65):
        print('{"jsonrpc":"2.0","method":"notifications/message"}', flush=True)
    sys.exit(0)
if mode == "disabled_capabilities":
    for index, method in enumerate(["ping", "sampling/createMessage", "roots/list", "elicitation/create"]):
        identifier = "server-" + str(index)
        print(json.dumps({"jsonrpc": "2.0", "id": identifier, "method": method, "params": {}}), flush=True)
        reply = json.loads(sys.stdin.readline())
        assert reply["id"] == identifier
        if method == "ping":
            assert reply == {"jsonrpc": "2.0", "id": identifier, "result": {}}
        else:
            assert reply["error"]["code"] == -32601 and "result" not in reply
initialization_result = {
    "protocolVersion": "2026-07-28" if mode == "wrong_version" else "2025-11-25",
    "capabilities": {"tools": {}},
    "serverInfo": {"name": "synthetic-tools", "version": "1"},
}
if mode == "duplicate_json":
    print('{"jsonrpc":"2.0","jsonrpc":"2.0","id":'
          + json.dumps(initialize["id"]) + ',"result":'
          + json.dumps(initialization_result) + '}', flush=True)
else:
    send(initialize["id"] + (1 if mode == "wrong_id" else 0), initialization_result)
assert json.loads(sys.stdin.readline())["method"] == "notifications/initialized"
listing = json.loads(sys.stdin.readline())
assert listing["method"] == "tools/list" and listing["params"] == {}
definition = {
    "name": "echo",
    "description": "Return a synthetic value; no host I/O",
    "inputSchema": {
        "type": "object", "properties": {"value": {"type": "integer"}},
        "required": ["value"], "additionalProperties": False,
    },
    "outputSchema": {
        "type": "object", "properties": {"value": {"type": "integer"}},
        "required": ["value"], "additionalProperties": False,
    },
}
if mode == "schema_ref":
    definition["inputSchema"] = {"$ref": "https://example.invalid/not-authorized"}
if mode == "schema_pattern":
    definition["inputSchema"] = {"type": "object", "properties": {"value": {"type": "string", "pattern": "(?=hostile)"}}}
if mode == "schema_drift":
    definition["description"] = "Changed after selection"
if mode == "list_changed":
    print('{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}', flush=True)
send(listing["id"], {"tools": [definition, definition] if mode == "duplicate_tool" else [definition]})
line = sys.stdin.readline()
if line:
    call = json.loads(line)
    assert call["method"] == "tools/call"
    if mode != "bad_arguments":
        assert call["params"] == {"name": "echo", "arguments": {"value": 42}}
    result = {"content": [{"type": "text", "text": "42"}],
              "structuredContent": {"value": 42}, "isError": False}
    if mode == "output_schema":
        result["structuredContent"] = {"value": "not an integer"}
    if mode == "embedded_resource":
        result["content"] = [{"type": "resource", "resource": {"uri": "file:///host/omitted", "text": "do not fetch"}}]
    if mode == "bad_is_error":
        result["isError"] = "false"
    if mode == "is_error":
        result["isError"] = True
        del result["structuredContent"]
    if mode == "resource_link":
        result["content"] = [{"type": "resource_link", "uri": "https://example.invalid/not-authorized"}]
    send(call["id"], result)
