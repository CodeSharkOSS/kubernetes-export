//! Cluster access via the local kubeconfig. Secrets are deliberately absent
//! from `KINDS` and `load_items` refuses to fetch them.

use crate::sanitize;
use anyhow::{Context, Result};
use kube::{
    Client, Config,
    api::{Api, DynamicObject, ListParams},
    core::{ApiResource, GroupVersionKind},
};
use serde_json::Value;
use std::collections::BTreeSet;

pub enum Msg {
    Namespaces(Result<(String, Vec<String>)>),
    Items(Result<(Vec<Item>, Vec<String>)>),
    Deploy(Evt),
    Prereq(Result<Prereq, String>),
    Folder(FolderFor, std::path::PathBuf),
}

#[derive(Clone, Copy, Debug)]
pub enum FolderFor {
    DeploySource,
    ExportDest,
}

// ---------------------------------------------------------------- deploy types

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    DryRun,
    Apply,
    Remove,
    Health,
    CreateNamespace,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Level {
    Ready,
    Progressing,
    Failed,
    Missing,
}

#[derive(Clone, Debug)]
pub struct Health {
    pub level: Level,
    pub detail: String,
}

pub struct Evt {
    /// Folder-load generation this event belongs to; stale events are ignored.
    pub epoch: u64,
    /// Index of the file in the deploy list (None for namespace actions).
    pub idx: Option<usize>,
    pub action: Action,
    pub result: Result<String, String>,
    pub health: Option<Health>,
}

pub struct Prereq {
    pub ns_exists: bool,
    /// Names only (metadata-only listing: Secret data is never returned). None = not permitted.
    pub secrets: Option<Vec<String>>,
}

pub struct Item {
    pub kind: &'static str,
    pub name: String,
    /// Cleaned and redacted object.
    pub value: Value,
    pub warnings: Vec<String>,
    pub selected: bool,
    /// Value as fetched (cleaned + redacted), for "Reset".
    pub original: Value,
    /// Text buffer of the YAML editor (created lazily).
    pub buf: Option<String>,
    pub err: Option<String>,
    pub edited: bool,
    /// Bumped on every change to `value`; lets caches know when to refresh.
    pub rev: u64,
    pub fold_edit: std::collections::BTreeSet<usize>,
    pub fold_out: std::collections::BTreeSet<usize>,
}

impl Item {
    pub fn new(kind: &'static str, name: String, value: Value, warnings: Vec<String>) -> Self {
        Self {
            kind,
            name,
            original: value.clone(),
            value,
            warnings,
            selected: true,
            buf: None,
            err: None,
            edited: false,
            rev: 0,
            fold_edit: BTreeSet::default(),
            fold_out: BTreeSet::default(),
        }
    }
}

/// (group, version, kind, plural). Secrets are intentionally NOT listed.
const KINDS: &[(&str, &str, &str, &str)] = &[
    ("", "v1", "ConfigMap", "configmaps"),
    ("", "v1", "ServiceAccount", "serviceaccounts"),
    ("", "v1", "PersistentVolumeClaim", "persistentvolumeclaims"),
    ("", "v1", "Service", "services"),
    ("apps", "v1", "Deployment", "deployments"),
    ("apps", "v1", "StatefulSet", "statefulsets"),
    ("apps", "v1", "DaemonSet", "daemonsets"),
    ("batch", "v1", "Job", "jobs"),
    ("batch", "v1", "CronJob", "cronjobs"),
    ("networking.k8s.io", "v1", "Ingress", "ingresses"),
    ("networking.k8s.io", "v1", "NetworkPolicy", "networkpolicies"),
    ("autoscaling", "v2", "HorizontalPodAutoscaler", "horizontalpodautoscalers"),
    ("policy", "v1", "PodDisruptionBudget", "poddisruptionbudgets"),
    ("rbac.authorization.k8s.io", "v1", "Role", "roles"),
    ("rbac.authorization.k8s.io", "v1", "RoleBinding", "rolebindings"),
];

pub async fn list_namespaces() -> Result<(String, Vec<String>)> {
    let cfg = Config::infer().await.context("reading local kubeconfig")?;
    let ctx = cfg.cluster_url.to_string();
    let default_ns = cfg.default_namespace.clone();
    let client = Client::try_from(cfg)?;
    let api: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(client);
    match api.list(&ListParams::default()).await {
        Ok(list) => {
            let mut names: Vec<String> = list.items.into_iter().filter_map(|n| n.metadata.name).collect();
            names.sort();
            Ok((ctx, names))
        }
        // Restricted RBAC: cluster-wide namespace listing is often forbidden.
        // Fall back to the kubeconfig's default namespace so the app stays usable.
        Err(_) => Ok((format!("{ctx} (can't list namespaces; using kubeconfig default)"), vec![default_ns])),
    }
}

pub async fn load_items(ns: &str) -> Result<(Vec<Item>, Vec<String>)> {
    let client = client().await.map_err(|e| anyhow::anyhow!("reading local kubeconfig: {e}"))?;
    let mut items = vec![];
    let mut notes = vec![];
    for &(group, version, kind, plural) in KINDS {
        assert_ne!(kind, "Secret", "refusing to fetch Secrets");
        let ar = ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(group, version, kind), plural);
        let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, &ar);
        let list = match api.list(&ListParams::default()).await {
            Ok(l) => l,
            Err(e) => {
                notes.push(format!("{kind}: skipped ({e})"));
                continue;
            }
        };
        for obj in list.items {
            let mut v = serde_json::to_value(&obj)?;
            // Owned by another controller (e.g. Jobs from CronJobs): skip.
            if v.pointer("/metadata/ownerReferences").is_some_and(|o| !o.is_null()) {
                continue;
            }
            let name = obj.metadata.name.clone().unwrap_or_default();
            if kind == "ConfigMap" && name == "kube-root-ca.crt" {
                continue;
            }
            if kind == "ServiceAccount" && name == "default" {
                continue;
            }
            v["apiVersion"] = Value::String(if group.is_empty() {
                version.into()
            } else {
                format!("{group}/{version}")
            });
            v["kind"] = Value::String(kind.into());
            sanitize::strip_cluster_fields(&mut v, kind);
            let warnings = sanitize::redact(&mut v, kind);
            items.push(Item::new(kind, name, v, warnings));
        }
    }
    Ok((items, notes))
}

// ------------------------------------------------------------- deploy: cluster ops

static CLIENT: tokio::sync::OnceCell<Client> = tokio::sync::OnceCell::const_new();

/// One client for the whole process. Building a client from kubeconfig can run
/// an external auth command (e.g. `aws eks get-token`), so don't do it per call.
pub async fn client() -> Result<Client, String> {
    CLIENT.get_or_try_init(|| async { Client::try_default().await.map_err(|e| e.to_string()) }).await.cloned()
}

pub const FIELD_MANAGER: &str = "k8s-namespace-exporter";

fn err_str(e: kube::Error) -> String {
    match e {
        kube::Error::Api(r) => format!("{}: {}", r.reason, r.message),
        other => other.to_string(),
    }
}

/// `ApiResource` for an exported manifest. Only the kinds this tool exports are
/// supported; Secrets are not (and never will be).
pub fn resource_for(api_version: &str, kind: &str) -> Option<ApiResource> {
    KINDS.iter().find_map(|&(g, v, k, plural)| {
        let av = if g.is_empty() { v.to_string() } else { format!("{g}/{v}") };
        (k == kind && av == api_version)
            .then(|| ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(g, v, k), plural))
    })
}

pub fn kind_static(kind: &str) -> Option<&'static str> {
    KINDS.iter().find(|k| k.2 == kind).map(|k| k.2)
}

/// The cluster API URL of the active kubeconfig context.
pub async fn cluster_url() -> Result<String, String> {
    Config::infer().await.map(|c| c.cluster_url.to_string()).map_err(|e| format!("reading local kubeconfig: {e}"))
}

/// True if this tool's field manager owns fields of the live object, i.e. it was
/// created/updated by this tool. Used to restrict what `remove` may delete.
pub async fn managed_by_us(ns: &str, api_version: &str, kind: &str, name: &str) -> Result<bool, String> {
    let ar = resource_for(api_version, kind).ok_or("unsupported kind")?;
    let api: Api<DynamicObject> = Api::namespaced_with(client().await?, ns, &ar);
    let Some(o) = api.get_opt(name).await.map_err(err_str)? else {
        return Err("not found in cluster".into());
    };
    Ok(o.metadata.managed_fields.iter().flatten().any(|m| m.manager.as_deref() == Some(FIELD_MANAGER)))
}

pub async fn prereqs(ns: &str) -> Result<Prereq, String> {
    use k8s_openapi::api::core::v1::{Namespace, Secret};
    let client = client().await?;
    let nsapi: Api<Namespace> = Api::all(client.clone());
    let ns_exists = nsapi.get_opt(ns).await.map_err(err_str)?.is_some();
    let secrets = if ns_exists {
        let api: Api<Secret> = Api::namespaced(client, ns);
        // list_metadata asks the API server for metadata only: no Secret data is transferred.
        api.list_metadata(&ListParams::default())
            .await
            .ok()
            .map(|l| l.items.into_iter().filter_map(|s| s.metadata.name).collect())
    } else {
        Some(vec![])
    };
    Ok(Prereq { ns_exists, secrets })
}

pub async fn create_namespace(ns: &str) -> Result<String, String> {
    use k8s_openapi::api::core::v1::Namespace;
    use kube::api::{Patch, PatchParams};
    let client = client().await?;
    let api: Api<Namespace> = Api::all(client);
    let body = serde_json::json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":ns}});
    api.patch(ns, &PatchParams::apply(FIELD_MANAGER), &Patch::Apply(&body)).await.map_err(err_str)?;
    Ok(format!("namespace '{ns}' ready"))
}

/// Server-side apply (idempotent). With `dry`, the API server validates and
/// admits the object but persists nothing.
pub async fn apply(ns: &str, obj: &Value, dry: bool) -> Result<String, String> {
    use kube::api::{Patch, PatchParams};
    let kind = obj["kind"].as_str().unwrap_or("");
    if kind == "Secret" {
        return Err("Secrets are never deployed by this tool".into());
    }
    let ar = resource_for(obj["apiVersion"].as_str().unwrap_or(""), kind)
        .ok_or_else(|| format!("unsupported kind/apiVersion: {kind}"))?;
    let name = obj["metadata"]["name"].as_str().ok_or("missing metadata.name")?;
    let client = client().await?;
    let api: Api<DynamicObject> = Api::namespaced_with(client, ns, &ar);
    let existed = api.get_opt(name).await.map_err(err_str)?.is_some();
    let mut pp = PatchParams::apply(FIELD_MANAGER);
    if dry {
        pp = pp.dry_run();
    }
    api.patch(name, &pp, &Patch::Apply(obj)).await.map_err(err_str)?;
    Ok(match (existed, dry) {
        (true, true) => "would update (dry run OK)".into(),
        (false, true) => "would create (dry run OK)".into(),
        (true, false) => "updated".into(),
        (false, false) => "created".into(),
    })
}

pub async fn remove(ns: &str, api_version: &str, kind: &str, name: &str) -> Result<String, String> {
    use kube::api::DeleteParams;
    if kind == "Secret" {
        return Err("Secrets are never touched by this tool".into());
    }
    let ar = resource_for(api_version, kind).ok_or("unsupported kind")?;
    let client = client().await?;
    let api: Api<DynamicObject> = Api::namespaced_with(client, ns, &ar);
    api.delete(name, &DeleteParams::default()).await.map_err(err_str)?;
    Ok("deleted".into())
}

const BAD_WAITING: &[&str] = &[
    "ImagePullBackOff", "ErrImagePull", "CrashLoopBackOff", "CreateContainerConfigError",
    "CreateContainerError", "InvalidImageName", "RunContainerError",
];

async fn pod_problem(client: Client, ns: &str, selector: &Value) -> Option<String> {
    use k8s_openapi::api::core::v1::Pod;
    let sel: Vec<String> = selector
        .get("matchLabels")?
        .as_object()?
        .iter()
        .filter_map(|(k, v)| Some(format!("{k}={}", v.as_str()?)))
        .collect();
    if sel.is_empty() {
        return None;
    }
    let api: Api<Pod> = Api::namespaced(client, ns);
    let pods = api.list(&ListParams::default().labels(&sel.join(","))).await.ok()?;
    for p in pods.items {
        let pname = p.metadata.name.clone().unwrap_or_default();
        let st = p.status?;
        let all = st.init_container_statuses.into_iter().flatten().chain(st.container_statuses.into_iter().flatten());
        for c in all {
            if let Some(w) = c.state.and_then(|s| s.waiting)
                && let Some(reason) = w.reason.filter(|r| BAD_WAITING.contains(&r.as_str()))
            {
                return Some(format!("{pname}: {reason} {}", w.message.unwrap_or_default()).trim().to_string());
            }
        }
        for cond in st.conditions.into_iter().flatten() {
            if cond.type_ == "PodScheduled" && cond.status == "False" {
                return Some(format!("{pname}: {} {}", cond.reason.unwrap_or_default(), cond.message.unwrap_or_default()).trim().to_string());
            }
        }
    }
    None
}

/// Live health of a deployed resource: replica readiness, plus the reason if
/// its pods are stuck (bad image, missing Secret/ConfigMap, unschedulable...).
pub async fn health(ns: &str, api_version: &str, kind: &str, name: &str) -> Result<Health, String> {
    let ar = resource_for(api_version, kind).ok_or("unsupported kind")?;
    let client = client().await?;
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, &ar);
    let Some(o) = api.get_opt(name).await.map_err(err_str)? else {
        return Ok(Health { level: Level::Missing, detail: "not in cluster".into() });
    };
    let (sp, st) = (&o.data["spec"], &o.data["status"]);
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let h = |level, detail: String| Ok(Health { level, detail });
    match kind {
        "Deployment" | "StatefulSet" | "DaemonSet" => {
            let (want, ready) = if kind == "DaemonSet" {
                (n(&st["desiredNumberScheduled"]), n(&st["numberReady"]))
            } else {
                (sp["replicas"].as_u64().unwrap_or(1), n(&st["readyReplicas"]))
            };
            let updated_ok = kind != "Deployment" || n(&st["updatedReplicas"]) >= want;
            if want == 0 {
                return h(Level::Ready, "scaled to 0".into());
            }
            if ready >= want && updated_ok {
                return h(Level::Ready, format!("{ready}/{want} ready"));
            }
            match pod_problem(client, ns, &sp["selector"]).await {
                Some(p) => h(Level::Failed, format!("{ready}/{want} ready - {p}")),
                None => h(Level::Progressing, format!("{ready}/{want} ready")),
            }
        }
        "Job" => {
            if n(&st["succeeded"]) > 0 {
                h(Level::Ready, "completed".into())
            } else if n(&st["failed"]) > 0 && n(&st["active"]) == 0 {
                h(Level::Failed, format!("{} failed attempt(s)", n(&st["failed"])))
            } else {
                h(Level::Progressing, "running".into())
            }
        }
        "PersistentVolumeClaim" => match st["phase"].as_str() {
            Some("Bound") => h(Level::Ready, "bound".into()),
            p => h(Level::Progressing, p.unwrap_or("pending").to_string()),
        },
        "CronJob" => h(Level::Ready, if sp["suspend"].as_bool() == Some(true) { "suspended".into() } else { "scheduled".into() }),
        _ => h(Level::Ready, "exists".into()),
    }
}
