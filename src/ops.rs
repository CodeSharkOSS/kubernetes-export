// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Headless operations shared by the CLI and the MCP server. Every function
//! takes plain arguments and returns JSON, so an agent (or script) can drive
//! exactly what the GUI does.
//!
//! Safety model for unattended use:
//!   * export / plan / check / health / map / dry-run only read the cluster.
//!   * Applying or removing requires `confirm_namespace` (== the namespace) AND
//!     `confirm_cluster` (== the cluster URL reported by `check`), so an agent
//!     can't hit the wrong kubeconfig context by accident.
//!   * Secrets are never exported, deployed or removed.

use crate::{common, export, folder, graph, k8s};
use clap::Args;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

type Res = Result<Value, String>;

fn ns_ok(ns: &str) -> Result<(), String> {
    if export::valid_ns(ns) { Ok(()) } else { Err(format!("'{ns}' is not a valid namespace name")) }
}

// ------------------------------------------------------------------- args

#[derive(Args, Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct ExportArgs {
    /// Source namespace to export.
    #[arg(long)]
    pub namespace: String,
    /// Output folder.
    #[arg(long)]
    pub out: String,
    /// Write `__NAMESPACE__` placeholders (default when no target namespace is given).
    #[arg(long)]
    pub template: bool,
    /// Rewrite the namespace to this one instead of templating.
    #[arg(long)]
    pub target_namespace: Option<String>,
    /// Only export these kinds (e.g. Deployment). Default: all.
    #[arg(long = "kind")]
    pub kinds: Vec<String>,
    /// Skip resources: `Kind/name` or `name`.
    #[arg(long)]
    pub exclude: Vec<String>,
    /// Shared value applied to every resource that has the node: `path=yaml`,
    /// e.g. `$pod.nodeSelector."eks.amazonaws.com/nodegroup"=grav`.
    #[arg(long = "set")]
    pub set: Vec<String>,
}

#[derive(Args, Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct DirArgs {
    /// Folder produced by `export`.
    #[arg(long)]
    pub dir: String,
    /// Target namespace (required for templated exports).
    #[arg(long)]
    pub namespace: Option<String>,
}

#[derive(Args, Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct DeployArgs {
    #[arg(long)]
    pub dir: String,
    #[arg(long)]
    pub namespace: String,
    /// Only these files (file name or `Kind/name`). Default: all, in order.
    #[arg(long)]
    pub only: Vec<String>,
    /// Actually apply. Without this, a server-side dry run is performed.
    #[arg(long)]
    pub apply: bool,
    /// Must equal --namespace when applying.
    #[arg(long)]
    pub confirm_namespace: Option<String>,
    /// Must equal the cluster URL reported by `check` when applying.
    #[arg(long)]
    pub confirm_cluster: Option<String>,
    /// Create the namespace if it doesn't exist (apply only).
    #[arg(long)]
    pub create_namespace: bool,
    /// After applying, wait up to this many seconds for resources to become healthy.
    #[arg(long, default_value_t = 0)]
    pub wait_secs: u64,
}

#[derive(Args, Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct RemoveArgs {
    #[arg(long)]
    pub dir: String,
    #[arg(long)]
    pub namespace: String,
    /// File name or `Kind/name` of the single resource to remove.
    #[arg(long)]
    pub file: String,
    #[arg(long)]
    pub confirm_namespace: Option<String>,
    #[arg(long)]
    pub confirm_cluster: Option<String>,
}

#[derive(Args, Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct HealthArgs {
    #[arg(long)]
    pub dir: String,
    #[arg(long)]
    pub namespace: String,
    #[arg(long)]
    pub only: Vec<String>,
}

#[derive(Args, Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct MapArgs {
    #[arg(long)]
    pub namespace: String,
}

// ------------------------------------------------------------------ helpers

fn matches_only(f: &folder::ManifestFile, only: &[String]) -> bool {
    only.is_empty() || only.iter().any(|o| *o == f.file || o.eq_ignore_ascii_case(&format!("{}/{}", f.kind, f.name)))
}

fn require_confirmation(ns: &str, confirm_ns: Option<&str>, confirm_cluster: Option<&str>, cluster: &str) -> Result<(), String> {
    if confirm_ns != Some(ns) {
        return Err(format!("refusing: confirm_namespace must be exactly '{ns}' to make changes"));
    }
    if confirm_cluster != Some(cluster) {
        return Err(format!("refusing: confirm_cluster must be exactly '{cluster}' (the cluster this kubeconfig points at) to make changes"));
    }
    Ok(())
}

fn file_json(f: &folder::ManifestFile) -> Value {
    json!({
        "file": f.file, "kind": f.kind, "name": f.name,
        "deployable": f.refused.is_none(), "refused": f.refused,
        "warnings": f.warnings, "secrets_referenced": f.secret_refs,
    })
}

fn load_folder(dir: &str) -> Result<folder::Folder, String> {
    folder::read(Path::new(dir))
}

fn target_ns(f: &folder::Folder, ns: Option<&str>) -> Result<String, String> {
    let ns = ns.map(str::to_owned).or_else(|| if f.templated { None } else { f.detected_ns.clone() }).ok_or("this folder is templated: pass the target namespace")?;
    ns_ok(&ns)?;
    Ok(ns)
}

// --------------------------------------------------------------- operations

pub async fn list_namespaces() -> Res {
    let (ctx, list) = k8s::list_namespaces().await.map_err(|e| format!("{e:#}"))?;
    let note = ctx.contains("can't list").then_some("namespace listing not permitted; showing the kubeconfig default");
    Ok(json!({"cluster": ctx.split(" (").next(), "namespaces": list, "note": note}))
}

pub async fn export(a: ExportArgs) -> Res {
    ns_ok(&a.namespace)?;
    let template = a.template || a.target_namespace.is_none();
    let mode = match (&a.target_namespace, template) {
        (Some(t), false) => {
            ns_ok(t)?;
            export::Mode::Replace(t.clone())
        }
        _ => export::Mode::Template,
    };
    let mut rules = vec![];
    for s in &a.set {
        let (path, value) = s.split_once('=').ok_or_else(|| format!("--set '{s}': expected path=value"))?;
        let r = common::Rule { enabled: true, path: path.trim().into(), value: value.into() };
        r.parsed().map_err(|e| format!("--set '{s}': {e}"))?;
        rules.push(r);
    }
    let (mut items, load_notes) = k8s::load_items(&a.namespace).await.map_err(|e| format!("{e:#}"))?;
    for it in &mut items {
        let kind_ok = a.kinds.is_empty() || a.kinds.iter().any(|k| k.eq_ignore_ascii_case(it.kind));
        let excluded = a.exclude.iter().any(|e| e == &it.name || e.eq_ignore_ascii_case(&format!("{}/{}", it.kind, it.name)));
        it.selected = kind_ok && !excluded;
    }
    let report = export::write_all(&items, &a.namespace, &mode, &rules, Path::new(&a.out)).map_err(|e| format!("{e:#}"))?;
    let mut secrets: Vec<String> = items
        .iter()
        .filter(|i| i.selected)
        .flat_map(|i| i.warnings.iter().filter_map(|w| w.strip_prefix("References Secret '").and_then(|r| r.split('\'').next()).map(str::to_string)))
        .collect();
    secrets.sort();
    secrets.dedup();
    Ok(json!({
        "namespace": a.namespace, "out": report.dir, "templated": report.templated,
        "exported": report.files.iter().map(|f| json!({"kind": f.kind, "name": f.resource, "file": f.file, "bytes": f.bytes, "notes": f.notes, "error": f.error})).collect::<Vec<_>>(),
        "ok": report.ok(), "failed": report.failed(),
        "deploy_script_error": report.script_error,
        "other_yaml_files_ignored": report.ignored,
        "secrets_to_create_in_target": secrets,
        "load_notes": load_notes,
        "secrets": "never exported",
    }))
}

pub async fn map(a: MapArgs) -> Res {
    ns_ok(&a.namespace)?;
    let (items, notes) = k8s::load_items(&a.namespace).await.map_err(|e| format!("{e:#}"))?;
    let g = graph::build(&items);
    let id = |i: usize| format!("{}/{}", g.nodes[i].kind, g.nodes[i].name);
    Ok(json!({
        "namespace": a.namespace,
        "nodes": g.nodes.iter().map(|n| json!({"id": format!("{}/{}", n.kind, n.name), "kind": n.kind, "name": n.name, "loaded": n.item.is_some(), "issue": n.issue})).collect::<Vec<_>>(),
        "edges": g.edges.iter().map(|e| json!({"from": id(e.from), "to": id(e.to), "relation": e.label})).collect::<Vec<_>>(),
        "problems": g.nodes.iter().filter_map(|n| n.issue.as_ref().map(|i| json!({"resource": format!("{}/{}", n.kind, n.name), "issue": i}))).collect::<Vec<_>>(),
        "load_notes": notes,
    }))
}

/// Offline: what's in the folder, in what order, and what it needs.
pub fn plan(a: &DirArgs) -> Res {
    let f = load_folder(&a.dir)?;
    if let Some(ns) = &a.namespace {
        ns_ok(ns)?;
    }
    Ok(json!({
        "dir": a.dir, "templated": f.templated, "detected_namespace": f.detected_ns,
        "namespace_required": f.templated && a.namespace.is_none(),
        "apply_order": f.files.iter().map(|x| &x.file).collect::<Vec<_>>(),
        "files": f.files.iter().map(file_json).collect::<Vec<_>>(),
        "secrets_needed": f.secrets_needed(),
        "errors": f.errors,
    }))
}

/// Read-only cluster check: which cluster, does the namespace exist, are the needed Secrets there (names only).
pub async fn check(a: DirArgs) -> Res {
    let f = load_folder(&a.dir)?;
    let ns = target_ns(&f, a.namespace.as_deref())?;
    let cluster = k8s::cluster_url().await?;
    let pre = k8s::prereqs(&ns).await?;
    let needed = f.secrets_needed();
    let (present, missing): (Vec<_>, Vec<_>) = match &pre.secrets {
        Some(have) => needed.iter().cloned().partition(|n| have.contains(n)),
        None => (vec![], vec![]),
    };
    Ok(json!({
        "cluster": cluster, "namespace": ns, "namespace_exists": pre.ns_exists,
        "secrets_needed": needed, "secrets_present": present, "secrets_missing": missing,
        "secrets_checkable": pre.secrets.is_some(),
        "note": "Only Secret names are looked up (metadata-only); contents are never read.",
    }))
}

pub async fn deploy(a: DeployArgs) -> Res {
    ns_ok(&a.namespace)?;
    let f = load_folder(&a.dir)?;
    let cluster = k8s::cluster_url().await?;
    if a.apply {
        require_confirmation(&a.namespace, a.confirm_namespace.as_deref(), a.confirm_cluster.as_deref(), &cluster)?;
    }
    let chosen: Vec<&folder::ManifestFile> = f.files.iter().filter(|x| matches_only(x, &a.only)).collect();
    if chosen.is_empty() {
        return Err("no files matched".into());
    }
    let pre = k8s::prereqs(&a.namespace).await?;
    let mut ns_created = false;
    if a.apply && !pre.ns_exists {
        if !a.create_namespace {
            return Err(format!("namespace '{}' doesn't exist; pass create_namespace to create it", a.namespace));
        }
        k8s::create_namespace(&a.namespace).await?;
        ns_created = true;
    }
    let mut results = vec![];
    let mut stopped = None;
    for x in &chosen {
        if stopped.is_some() {
            results.push(json!({"file": x.file, "kind": x.kind, "name": x.name, "status": "skipped"}));
            continue;
        }
        if let Some(why) = &x.refused {
            results.push(json!({"file": x.file, "kind": x.kind, "name": x.name, "status": "refused", "message": why}));
            stopped = Some(x.file.clone());
            continue;
        }
        let r = match folder::manifest(&x.text, &a.namespace) {
            Ok(obj) => k8s::apply(&a.namespace, &obj, !a.apply).await,
            Err(e) => Err(e),
        };
        match r {
            Ok(m) => results.push(json!({"file": x.file, "kind": x.kind, "name": x.name, "status": "ok", "message": m})),
            Err(m) => {
                results.push(json!({"file": x.file, "kind": x.kind, "name": x.name, "status": "failed", "message": m}));
                stopped = Some(x.file.clone());
            }
        }
    }
    let health = if a.apply && a.wait_secs > 0 && stopped.is_none() { Some(wait_healthy(&a.namespace, &chosen, a.wait_secs).await) } else { None };
    Ok(json!({
        "mode": if a.apply { "apply" } else { "dry-run" }, "cluster": cluster, "namespace": a.namespace,
        "namespace_created": ns_created, "results": results, "stopped_at": stopped, "health": health,
        "next": if !a.apply && stopped.is_none() { "Dry run passed. To apply: set apply=true with confirm_namespace and confirm_cluster." } else { "" },
    }))
}

async fn health_of(ns: &str, x: &folder::ManifestFile) -> Value {
    match k8s::health(ns, &x.api_version, x.kind, &x.name).await {
        Ok(h) => json!({"file": x.file, "kind": x.kind, "name": x.name, "level": format!("{:?}", h.level), "detail": h.detail}),
        Err(e) => json!({"file": x.file, "kind": x.kind, "name": x.name, "level": "Error", "detail": e}),
    }
}

async fn wait_healthy(ns: &str, files: &[&folder::ManifestFile], secs: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let mut all = vec![];
        for x in files.iter().filter(|x| x.refused.is_none()) {
            all.push(health_of(ns, x).await);
        }
        let done = all.iter().all(|h| h["level"] == "Ready");
        let failed = all.iter().any(|h| h["level"] == "Failed");
        if done || failed || Instant::now() >= deadline {
            return json!({"all_ready": done, "any_failed": failed, "resources": all});
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

pub async fn health(a: HealthArgs) -> Res {
    ns_ok(&a.namespace)?;
    let f = load_folder(&a.dir)?;
    let mut out = vec![];
    for x in f.files.iter().filter(|x| matches_only(x, &a.only) && x.refused.is_none()) {
        out.push(health_of(&a.namespace, x).await);
    }
    let ready = out.iter().all(|h| h["level"] == "Ready");
    Ok(json!({"namespace": a.namespace, "all_ready": ready, "resources": out}))
}

/// Remove one resource, only if this tool created/updated it.
pub async fn remove(a: RemoveArgs) -> Res {
    ns_ok(&a.namespace)?;
    let cluster = k8s::cluster_url().await?;
    require_confirmation(&a.namespace, a.confirm_namespace.as_deref(), a.confirm_cluster.as_deref(), &cluster)?;
    let f = load_folder(&a.dir)?;
    let x = f
        .files
        .iter()
        .find(|x| matches_only(x, std::slice::from_ref(&a.file)))
        .ok_or_else(|| format!("no file matching '{}'", a.file))?;
    if x.refused.is_some() {
        return Err("refusing: this file is not deployable by this tool".into());
    }
    if !k8s::managed_by_us(&a.namespace, &x.api_version, x.kind, &x.name).await? {
        return Err(format!("refusing: {}/{} was not created by this tool", x.kind, x.name));
    }
    let m = k8s::remove(&a.namespace, &x.api_version, x.kind, &x.name).await?;
    Ok(json!({"cluster": cluster, "namespace": a.namespace, "removed": format!("{}/{}", x.kind, x.name), "message": m}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with_files() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("k8s-ops-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let items = vec![
            k8s::Item::new("ConfigMap", "cfg".into(), json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"cfg","namespace":"dev"}}), vec![]),
            k8s::Item::new("Deployment", "app".into(), json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"app","namespace":"dev"},
                "spec":{"template":{"spec":{"containers":[{"name":"c","envFrom":[{"secretRef":{"name":"creds"}}]}]}}}}), vec![]),
        ];
        export::write_all(&items, "dev", &export::Mode::Template, &[], &dir).unwrap();
        dir
    }

    #[test]
    fn plan_is_offline_ordered_and_lists_secrets() {
        let dir = dir_with_files();
        let p = plan(&DirArgs { dir: dir.display().to_string(), namespace: None }).unwrap();
        assert_eq!(p["templated"], true);
        assert_eq!(p["namespace_required"], true);
        assert_eq!(p["apply_order"], json!(["configmap-cfg.yaml", "deployment-app.yaml"]));
        assert_eq!(p["secrets_needed"], json!(["creds"]));
        let bad = plan(&DirArgs { dir: dir.display().to_string(), namespace: Some("Bad NS".into()) });
        assert!(bad.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirmation_is_exact() {
        assert!(require_confirmation("qa", None, None, "https://c").is_err());
        assert!(require_confirmation("qa", Some("qa"), None, "https://c").is_err());
        assert!(require_confirmation("qa", Some("qa"), Some("https://other"), "https://c").is_err());
        assert!(require_confirmation("qa", Some("qa"), Some("https://c"), "https://c").is_ok());
        assert!(require_confirmation("qa", Some("QA"), Some("https://c"), "https://c").is_err());
    }

    #[test]
    fn selection_by_file_or_kind_name() {
        let f = folder::ManifestFile { file: "deployment-app.yaml".into(), kind: "Deployment", api_version: "apps/v1".into(), name: "app".into(), text: String::new(), refused: None, warnings: vec![], secret_refs: vec![] };
        assert!(matches_only(&f, &[]));
        assert!(matches_only(&f, &["deployment-app.yaml".into()]));
        assert!(matches_only(&f, &["deployment/app".into()]));
        assert!(!matches_only(&f, &["service/app".into()]));
    }

    #[test]
    fn unknown_argument_fields_are_rejected() {
        let r: Result<DeployArgs, _> = serde_json::from_value(json!({"dir":"x","namespace":"qa","aply":true}));
        assert!(r.is_err(), "a typo like `aply` must not silently become a dry run");
    }
}
