//! Builds a connection graph of the loaded resources purely from their
//! manifests (pods aren't loaded, so Service -> workload links come from
//! selector / template-label matching).

use crate::{common, k8s::Item};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

pub struct Node {
    pub kind: String,
    pub name: String,
    /// Index into the loaded items; None for referenced-but-not-loaded nodes.
    pub item: Option<usize>,
    pub ghost: bool,
    pub issue: Option<String>,
}

pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub label: &'static str,
}

#[derive(Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

struct Builder {
    g: Graph,
    idx: HashMap<(String, String), usize>,
}

impl Builder {
    fn node(&mut self, kind: &str, name: &str) -> usize {
        if let Some(&i) = self.idx.get(&(kind.to_string(), name.to_string())) {
            return i;
        }
        self.g.nodes.push(Node { kind: kind.into(), name: name.into(), item: None, ghost: true, issue: None });
        let i = self.g.nodes.len() - 1;
        self.idx.insert((kind.into(), name.into()), i);
        i
    }
    fn edge(&mut self, from: usize, to: usize, label: &'static str) {
        if from != to && !self.g.edges.iter().any(|e| e.from == from && e.to == to && e.label == label) {
            self.g.edges.push(Edge { from, to, label });
        }
    }
    fn link(&mut self, from: usize, kind: &str, name: Option<&str>, label: &'static str) {
        if let Some(n) = name.filter(|n| !n.is_empty()) {
            let t = self.node(kind, n);
            self.edge(from, t, label);
        }
    }
}

fn s<'a>(v: &'a Value, ptr: &str) -> Option<&'a str> {
    v.pointer(ptr).and_then(Value::as_str)
}

fn str_map(v: Option<&Value>) -> BTreeMap<String, String> {
    v.and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
        .unwrap_or_default()
}

fn selects(sel: &BTreeMap<String, String>, labels: &BTreeMap<String, String>) -> bool {
    !sel.is_empty() && sel.iter().all(|(k, v)| labels.get(k) == Some(v))
}

fn array<'a>(v: &'a Value, ptr: &str) -> &'a [Value] {
    v.pointer(ptr).and_then(Value::as_array).map_or(&[][..], Vec::as_slice)
}

fn pod_refs(b: &mut Builder, from: usize, spec: &Value) {
    for group in ["containers", "initContainers"] {
        for c in array(spec, &format!("/{group}")) {
            for e in array(c, "/env") {
                b.link(from, "ConfigMap", s(e, "/valueFrom/configMapKeyRef/name"), "env");
                b.link(from, "Secret", s(e, "/valueFrom/secretKeyRef/name"), "env");
            }
            for e in array(c, "/envFrom") {
                b.link(from, "ConfigMap", s(e, "/configMapRef/name"), "env");
                b.link(from, "Secret", s(e, "/secretRef/name"), "env");
            }
        }
    }
    for v in array(spec, "/volumes") {
        b.link(from, "ConfigMap", s(v, "/configMap/name"), "volume");
        b.link(from, "Secret", s(v, "/secret/secretName"), "volume");
        b.link(from, "PersistentVolumeClaim", s(v, "/persistentVolumeClaim/claimName"), "volume");
    }
    for p in array(spec, "/imagePullSecrets") {
        b.link(from, "Secret", s(p, "/name"), "pull secret");
    }
    if let Some(sa) = s(spec, "/serviceAccountName").filter(|n| *n != "default") {
        b.link(from, "ServiceAccount", Some(sa), "runs as");
    }
}

pub fn build(items: &[Item]) -> Graph {
    let mut b = Builder { g: Graph::default(), idx: HashMap::new() };
    for (i, it) in items.iter().enumerate() {
        let n = b.node(it.kind, &it.name);
        b.g.nodes[n].ghost = false;
        b.g.nodes[n].item = Some(i);
    }
    // workloads and their pod-template labels
    let mut workloads: Vec<(usize, BTreeMap<String, String>)> = vec![];
    for it in items {
        if let Some(pp) = common::pod_path(it.kind) {
            let tmpl = pp.trim_end_matches("/spec");
            let labels = str_map(it.value.pointer(&format!("{tmpl}/metadata/labels")));
            workloads.push((b.node(it.kind, &it.name), labels));
        }
    }
    let covers = |b: &mut Builder, from: usize, sel: &BTreeMap<String, String>, label: &'static str| -> usize {
        let mut n = 0;
        for (w, labels) in &workloads {
            if selects(sel, labels) {
                b.edge(from, *w, label);
                n += 1;
            }
        }
        n
    };

    for it in items {
        let v = &it.value;
        let me = b.node(it.kind, &it.name);
        if let Some(spec) = common::pod_path(it.kind).and_then(|pp| v.pointer(pp)) {
            pod_refs(&mut b, me, spec);
        }
        match it.kind {
            "StatefulSet" => b.link(me, "Service", s(v, "/spec/serviceName"), "governing service"),
            "Service" => {
                let sel = str_map(v.pointer("/spec/selector"));
                if covers(&mut b, me, &sel, "selects") == 0 && !sel.is_empty() {
                    b.g.nodes[me].issue = Some("Selector matches no workload in this namespace".into());
                }
            }
            "Ingress" => {
                b.link(me, "Service", s(v, "/spec/defaultBackend/service/name"), "routes to");
                for r in array(v, "/spec/rules") {
                    for p in array(r, "/http/paths") {
                        b.link(me, "Service", s(p, "/backend/service/name"), "routes to");
                    }
                }
                for t in array(v, "/spec/tls") {
                    b.link(me, "Secret", s(t, "/secretName"), "tls");
                }
            }
            "HorizontalPodAutoscaler" => {
                if let (Some(k), Some(n)) = (s(v, "/spec/scaleTargetRef/kind"), s(v, "/spec/scaleTargetRef/name")) {
                    b.link(me, k, Some(n), "scales");
                }
            }
            "PodDisruptionBudget" => {
                covers(&mut b, me, &str_map(v.pointer("/spec/selector/matchLabels")), "protects");
            }
            "NetworkPolicy" => {
                covers(&mut b, me, &str_map(v.pointer("/spec/podSelector/matchLabels")), "applies to");
            }
            "RoleBinding" => {
                if let (Some(k), Some(n)) = (s(v, "/roleRef/kind"), s(v, "/roleRef/name")) {
                    b.link(me, k, Some(n), "binds");
                }
                for sub in array(v, "/subjects") {
                    if s(sub, "/kind") == Some("ServiceAccount") {
                        b.link(me, "ServiceAccount", s(sub, "/name"), "grants to");
                    }
                }
            }
            _ => {}
        }
    }
    for n in b.g.nodes.iter_mut().filter(|n| n.ghost) {
        if !matches!(n.kind.as_str(), "Secret" | "ClusterRole") {
            n.issue = Some("Referenced, but not found in this namespace".into());
        }
    }
    b.g
}

// ------------------------------------------------------------------ layout

pub const NODE_W: f32 = 200.0;
pub const NODE_H: f32 = 32.0;
const GAP_X: f32 = 130.0;
const GAP_Y: f32 = 12.0;
pub const HEADER_H: f32 = 28.0;

pub struct Layout {
    /// Top-left of each node in world coordinates.
    pub pos: Vec<[f32; 2]>,
    pub col_of: Vec<usize>,
    /// (x, title) per column.
    pub cols: Vec<(f32, String)>,
    pub size: [f32; 2],
}

fn tier(kind: &str) -> usize {
    match kind {
        "Ingress" => 0,
        "Service" => 1,
        "Deployment" | "StatefulSet" | "DaemonSet" | "Job" | "CronJob" => 2,
        "HorizontalPodAutoscaler" | "PodDisruptionBudget" | "NetworkPolicy" => 3,
        "ConfigMap" | "PersistentVolumeClaim" | "Secret" => 4,
        "ServiceAccount" => 5,
        "RoleBinding" => 6,
        _ => 7, // Role, ClusterRole, anything else
    }
}

pub fn layout(g: &Graph) -> Layout {
    let mut tiers: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, n) in g.nodes.iter().enumerate() {
        tiers.entry(tier(&n.kind)).or_default().push(i);
    }
    let mut cols: Vec<Vec<usize>> = tiers.into_values().collect();
    for c in &mut cols {
        c.sort_by(|&a, &b| (&g.nodes[a].kind, &g.nodes[a].name).cmp(&(&g.nodes[b].kind, &g.nodes[b].name)));
    }
    let mut col_of = vec![0; g.nodes.len()];
    for (ci, c) in cols.iter().enumerate() {
        for &n in c {
            col_of[n] = ci;
        }
    }
    // Barycenter sweeps to reduce edge crossings.
    let mut frac = vec![0.0f32; g.nodes.len()];
    let refresh = |cols: &Vec<Vec<usize>>, frac: &mut Vec<f32>| {
        for c in cols {
            for (r, &n) in c.iter().enumerate() {
                frac[n] = (r as f32 + 0.5) / c.len() as f32;
            }
        }
    };
    refresh(&cols, &mut frac);
    for _ in 0..4 {
        for ci in 0..cols.len() {
            let mut key: HashMap<usize, f32> = HashMap::new();
            for &n in &cols[ci] {
                let nb: Vec<f32> = g
                    .edges
                    .iter()
                    .filter_map(|e| {
                        if e.from == n { Some(e.to) } else if e.to == n { Some(e.from) } else { None }
                    })
                    .filter(|&o| col_of[o] != ci)
                    .map(|o| frac[o])
                    .collect();
                key.insert(n, if nb.is_empty() { frac[n] } else { nb.iter().sum::<f32>() / nb.len() as f32 });
            }
            cols[ci].sort_by(|a, b| key[a].total_cmp(&key[b]));
            refresh(&cols, &mut frac);
        }
    }
    let max_n = cols.iter().map(Vec::len).max().unwrap_or(0) as f32;
    let max_h = max_n * (NODE_H + GAP_Y);
    let mut pos = vec![[0.0, 0.0]; g.nodes.len()];
    let mut titles = vec![];
    for (ci, c) in cols.iter().enumerate() {
        let x = ci as f32 * (NODE_W + GAP_X);
        let y0 = HEADER_H + (max_h - c.len() as f32 * (NODE_H + GAP_Y)) / 2.0;
        for (r, &n) in c.iter().enumerate() {
            pos[n] = [x, y0 + r as f32 * (NODE_H + GAP_Y)];
        }
        let mut kinds: Vec<&str> = c.iter().map(|&n| g.nodes[n].kind.as_str()).collect();
        kinds.sort_unstable();
        kinds.dedup();
        kinds.truncate(3);
        titles.push((x, kinds.join(" / ")));
    }
    Layout {
        pos,
        col_of,
        cols: titles,
        size: [cols.len() as f32 * (NODE_W + GAP_X) - GAP_X, HEADER_H + max_h],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(kind: &'static str, name: &str, value: Value) -> Item {
        Item::new(kind, name.into(), value, vec![])
    }

    #[test]
    fn connects_resources() {
        let items = vec![
            item("Ingress", "web", json!({"spec":{"rules":[{"http":{"paths":[{"backend":{"service":{"name":"api"}}},{"backend":{"service":{"name":"gone"}}}]}}]}})),
            item("Service", "api", json!({"spec":{"selector":{"app":"api"}}})),
            item("Service", "orphan", json!({"spec":{"selector":{"app":"nothing"}}})),
            item("Deployment", "api", json!({"spec":{"template":{"metadata":{"labels":{"app":"api","x":"y"}},
                "spec":{"serviceAccountName":"sa","containers":[{"name":"c","env":[
                    {"name":"A","valueFrom":{"secretKeyRef":{"name":"creds","key":"k"}}}],
                    "envFrom":[{"configMapRef":{"name":"cfg"}}]}]}}}})),
            item("ConfigMap", "cfg", json!({})),
            item("HorizontalPodAutoscaler", "hpa", json!({"spec":{"scaleTargetRef":{"kind":"Deployment","name":"api"}}})),
        ];
        let g = build(&items);
        let id = |k: &str, n: &str| g.nodes.iter().position(|x| x.kind == k && x.name == n).unwrap();
        let has = |a: usize, b: usize, l: &str| g.edges.iter().any(|e| e.from == a && e.to == b && e.label == l);
        assert!(has(id("Ingress", "web"), id("Service", "api"), "routes to"));
        assert!(has(id("Service", "api"), id("Deployment", "api"), "selects"));
        assert!(has(id("Deployment", "api"), id("ConfigMap", "cfg"), "env"));
        assert!(has(id("Deployment", "api"), id("Secret", "creds"), "env"));
        assert!(has(id("HorizontalPodAutoscaler", "hpa"), id("Deployment", "api"), "scales"));
        assert!(g.nodes[id("Service", "orphan")].issue.is_some());
        assert!(g.nodes[id("Service", "gone")].issue.is_some());
        assert!(g.nodes[id("Secret", "creds")].issue.is_none() && g.nodes[id("Secret", "creds")].ghost);
        let l = layout(&g);
        assert_eq!(l.pos.len(), g.nodes.len());
    }
}
