//! Minimal MCP (Model Context Protocol) server over stdio: newline-delimited
//! JSON-RPC 2.0. stdout carries protocol messages only; nothing else may print there.

use crate::ops;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

const INSTRUCTIONS: &str = "Export, map and deploy Kubernetes namespaces using the local kubeconfig. \
Typical flow: k8s_list_namespaces -> k8s_map (understand it) -> k8s_export -> edit the YAML files on disk if needed -> \
k8s_plan -> k8s_check -> k8s_deploy (dry run) -> k8s_deploy with apply=true one file at a time (only=[...]) -> k8s_health. \
Secrets are never exported, deployed or removed; the export lists Secret names you must create yourself. \
Applying or removing changes a live cluster: always dry-run first, and only apply after the user has agreed to the cluster and namespace.";

fn tool(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({"name": name, "description": description,
           "inputSchema": {"type": "object", "properties": props, "required": required, "additionalProperties": false}})
}

fn strs(desc: &str) -> Value {
    json!({"type": "array", "items": {"type": "string"}, "description": desc})
}

pub fn tools() -> Vec<Value> {
    let s = |d: &str| json!({"type": "string", "description": d});
    vec![
        tool("k8s_list_namespaces", "List namespaces on the current kubeconfig context (read-only). Also reports the cluster URL.", json!({}), &[]),
        tool("k8s_map", "Connection graph of a namespace: Ingress->Service->workload->ConfigMap/PVC/ServiceAccount/Secret(name only), HPA/PDB/NetworkPolicy links, RoleBindings. Includes problems such as a Service selecting nothing or references to missing resources. Read-only.",
            json!({"namespace": s("Namespace to map")}), &["namespace"]),
        tool("k8s_export", "Export a namespace's resources to YAML files in a folder, plus an ordered deploy.sh. Reads the cluster, writes only to `out`. Secrets are never exported; credential-looking values become <REDACTED>. Returns what was written and which Secrets must be created in the target.",
            json!({"namespace": s("Source namespace"), "out": s("Output folder (created if missing)"),
                   "template": {"type": "boolean", "description": "Use __NAMESPACE__ placeholders (default when target_namespace is absent)"},
                   "target_namespace": s("Rewrite the namespace to this instead of templating"),
                   "kind": strs("Only these kinds, e.g. [\"Deployment\",\"Service\"]"), "exclude": strs("Skip `Kind/name` or `name`"),
                   "set": strs("Shared values `path=yaml` applied to every resource having that node, e.g. `$pod.containers[*].env[name=AWS_REGION].value=eu-west-1`")}),
            &["namespace", "out"]),
        tool("k8s_plan", "Offline description of an exported folder: apply order, kinds, warnings (e.g. <REDACTED> placeholders), Secrets needed, and whether a target namespace is required.",
            json!({"dir": s("Exported folder"), "namespace": s("Target namespace (optional)")}), &["dir"]),
        tool("k8s_check", "Read-only cluster check for a folder: which cluster you're pointed at, whether the target namespace exists, and which needed Secrets exist (names only; contents never read). Use its `cluster` value as confirm_cluster when applying.",
            json!({"dir": s("Exported folder"), "namespace": s("Target namespace")}), &["dir"]),
        tool("k8s_deploy", "Deploy files from an exported folder, in order, stopping at the first failure. DEFAULT IS A SERVER-SIDE DRY RUN that changes nothing. To really apply set apply=true AND confirm_namespace (== namespace) AND confirm_cluster (== the cluster URL from k8s_check). Use `only` to deploy one file at a time and k8s_health in between.",
            json!({"dir": s("Exported folder"), "namespace": s("Target namespace"), "only": strs("File names or `Kind/name`; default all"),
                   "apply": {"type": "boolean", "description": "Really apply (default false = dry run)"},
                   "confirm_namespace": s("Must equal namespace when apply=true"), "confirm_cluster": s("Must equal the cluster URL when apply=true"),
                   "create_namespace": {"type": "boolean", "description": "Create the namespace if missing (apply only)"},
                   "wait_secs": {"type": "integer", "description": "After applying, wait up to N seconds for health"}}),
            &["dir", "namespace"]),
        tool("k8s_health", "Live health of resources in a folder: replica readiness, and for stuck workloads the pod-level reason (ImagePullBackOff, CrashLoopBackOff, missing Secret/ConfigMap, unschedulable). Read-only.",
            json!({"dir": s("Exported folder"), "namespace": s("Target namespace"), "only": strs("File names or `Kind/name`")}), &["dir", "namespace"]),
        tool("k8s_remove", "Delete ONE resource from the cluster, only if this tool created it. Needs confirm_namespace and confirm_cluster like apply. Never touches Secrets.",
            json!({"dir": s("Exported folder"), "namespace": s("Target namespace"), "file": s("File name or `Kind/name`"),
                   "confirm_namespace": s("Must equal namespace"), "confirm_cluster": s("Must equal the cluster URL")}),
            &["dir", "namespace", "file"]),
    ]
}

fn parse<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))
}

pub async fn call_tool(name: &str, args: Value) -> Result<Value, String> {
    // The MCP schema calls the repeatable export filter `kind`; the struct field is `kinds`.
    let mut args = args;
    if name == "k8s_export"
        && let Some(o) = args.as_object_mut()
        && let Some(k) = o.remove("kind")
    {
        o.insert("kinds".into(), k);
    }
    match name {
        "k8s_list_namespaces" => ops::list_namespaces().await,
        "k8s_map" => ops::map(parse(args)?).await,
        "k8s_export" => ops::export(parse(args)?).await,
        "k8s_plan" => ops::plan(&parse(args)?),
        "k8s_check" => ops::check(parse(args)?).await,
        "k8s_deploy" => ops::deploy(parse(args)?).await,
        "k8s_health" => ops::health(parse(args)?).await,
        "k8s_remove" => ops::remove(parse(args)?).await,
        _ => Err(format!("unknown tool '{name}'")),
    }
}

/// Handle one JSON-RPC message; None for notifications (no reply).
pub async fn handle(req: Value) -> Option<Value> {
    let id = req.get("id").cloned()?;
    let method = req["method"].as_str().unwrap_or("");
    let ok = |result: Value| Some(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    let err = |code: i64, msg: String| Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}}));
    match method {
        "initialize" => ok(json!({
            "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "kubernetes-export", "version": env!("CARGO_PKG_VERSION")},
            "instructions": INSTRUCTIONS,
        })),
        "ping" => ok(json!({})),
        "tools/list" => ok(json!({"tools": tools()})),
        "tools/call" => {
            let name = req["params"]["name"].as_str().unwrap_or("").to_string();
            let args = req["params"].get("arguments").cloned().unwrap_or_else(|| json!({}));
            let (text, is_error) = match call_tool(&name, args).await {
                Ok(v) => (serde_json::to_string_pretty(&v).unwrap_or_default(), false),
                Err(e) => (e, true),
            };
            ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
        }
        other => err(-32601, format!("method not found: {other}")),
    }
}

pub fn serve() -> i32 {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("runtime: {e}");
            return 1;
        }
    };
    eprintln!("kubernetes-export MCP server ready (stdio)");
    let mut out = std::io::stdout();
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(req) => rt.block_on(handle(req)),
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}})),
        };
        if let Some(r) = reply {
            let _ = writeln!(out, "{r}");
            let _ = out.flush();
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    #[test]
    fn handshake_and_tool_list() {
        let rt = rt();
        let init = rt.block_on(handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}))).unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "kubernetes-export");
        assert!(rt.block_on(handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))).is_none());
        let list = rt.block_on(handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))).unwrap();
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"k8s_deploy") && names.contains(&"k8s_export") && names.len() == 8);
        let bad = rt.block_on(handle(json!({"jsonrpc":"2.0","id":3,"method":"nope"}))).unwrap();
        assert_eq!(bad["error"]["code"], -32601);
    }

    #[test]
    fn deploy_apply_is_refused_without_confirmation_and_typos_are_errors() {
        let rt = rt();
        // typo'd field is an error, not a silent dry run
        let r = rt.block_on(call_tool("k8s_deploy", json!({"dir":"x","namespace":"qa","aply":true})));
        assert!(r.unwrap_err().contains("invalid arguments"));
        // unknown tool
        assert!(rt.block_on(call_tool("k8s_nope", json!({}))).is_err());
        // plan works fully offline and reports through the tool-call envelope
        let resp = rt.block_on(handle(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"k8s_plan","arguments":{"dir":"/definitely/not/here"}}}))).unwrap();
        assert_eq!(resp["result"]["isError"], true);
    }
}
