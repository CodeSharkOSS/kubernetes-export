//! "Common" rules: set the value of a node (identified by path) across every
//! resource in which that node exists. Applied as an overlay at render/export
//! time, so per-resource edits and the original cluster values stay intact.
//!
//! Path syntax:  `$pod.containers[*].env[name=AWS_REGION].value`
//!   `$pod`        pod spec of any workload (Deployment/StatefulSet/DaemonSet/Job/CronJob)
//!   `key`, `"quoted.key/with.dots"`
//!   `[*]`         every array element / object value
//!   `[name=foo]`  array element whose `name` == foo (created if missing and more keys follow)
//!   `[0]`         array index

use crate::sanitize;
use std::fmt::Write as _;
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum Seg {
    Pod,
    Key(String),
    Any,
    Idx(usize),
    Match(String, String),
}

#[derive(Clone)]
pub struct Rule {
    pub enabled: bool,
    pub path: String,
    /// YAML text of the value to set.
    pub value: String,
}

impl Rule {
    pub fn parsed(&self) -> Result<(Vec<Seg>, Value), String> {
        let segs = parse(&self.path).ok_or("bad path")?;
        if segs.is_empty() {
            return Err("empty path".into());
        }
        let v: Value = serde_yaml_ng::from_str(&self.value).map_err(|e| format!("value: {e}"))?;
        Ok((segs, v))
    }
}

pub fn pod_path(kind: &str) -> Option<&'static str> {
    match kind {
        "Deployment" | "StatefulSet" | "DaemonSet" | "Job" => Some("/spec/template/spec"),
        "CronJob" => Some("/spec/jobTemplate/spec/template/spec"),
        _ => None,
    }
}

fn plain(k: &str) -> bool {
    !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn to_path(segs: &[Seg]) -> String {
    let mut out = String::new();
    for s in segs {
        match s {
            Seg::Pod => out.push_str("$pod"),
            Seg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                if plain(k) {
                    out.push_str(k);
                } else {
                    let _ = write!(out, "\"{k}\"");
                }
            }
            Seg::Any => out.push_str("[*]"),
            Seg::Idx(i) => { let _ = write!(out, "[{i}]"); },
            Seg::Match(k, v) => { let _ = write!(out, "[{k}={v}]"); },
        }
    }
    out
}

/// Replace the pod-spec prefix of a full path with `$pod`.
pub fn compress(kind: &str, segs: &[Seg]) -> Vec<Seg> {
    if let Some(p) = pod_path(kind) {
        let prefix: Vec<Seg> = p[1..].split('/').map(|k| Seg::Key(k.into())).collect();
        if segs.starts_with(&prefix) {
            let mut v = vec![Seg::Pod];
            v.extend_from_slice(&segs[prefix.len()..]);
            return v;
        }
    }
    segs.to_vec()
}

pub fn parse(s: &str) -> Option<Vec<Seg>> {
    let mut segs = vec![];
    let mut rest = s.trim();
    if let Some(r) = rest.strip_prefix("$pod") {
        segs.push(Seg::Pod);
        rest = r;
    }
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            rest = r;
        } else if let Some(r) = rest.strip_prefix('[') {
            let end = r.find(']')?;
            let inner = &r[..end];
            segs.push(if inner == "*" {
                Seg::Any
            } else if let Ok(i) = inner.parse() {
                Seg::Idx(i)
            } else {
                let (k, v) = inner.split_once('=')?;
                Seg::Match(k.into(), v.into())
            });
            rest = &r[end + 1..];
        } else if let Some(r) = rest.strip_prefix('"') {
            let end = r.find('"')?;
            segs.push(Seg::Key(r[..end].into()));
            rest = &r[end + 1..];
        } else {
            let end = rest.find(['.', '[']).unwrap_or(rest.len());
            segs.push(Seg::Key(rest[..end].into()));
            rest = &rest[end..];
        }
    }
    Some(segs)
}

/// Returns number of nodes set.
fn apply(v: &mut Value, segs: &[Seg], val: &Value) -> usize {
    let Some((first, rest)) = segs.split_first() else {
        *v = val.clone();
        return 1;
    };
    match first {
        Seg::Pod => 0,
        Seg::Key(k) => {
            let Some(m) = v.as_object_mut() else { return 0 };
            if let Some(child) = m.get_mut(k) {
                return apply(child, rest, val);
            }
            match rest.first() {
                None => {
                    m.insert(k.clone(), val.clone());
                    1
                }
                Some(Seg::Key(_)) => {
                    let mut child = Value::Object(Map::new());
                    let n = apply(&mut child, rest, val);
                    if n > 0 {
                        m.insert(k.clone(), child);
                    }
                    n
                }
                _ => 0,
            }
        }
        Seg::Any => match v {
            Value::Array(a) => a.iter_mut().map(|x| apply(x, rest, val)).sum(),
            Value::Object(m) => m.values_mut().map(|x| apply(x, rest, val)).sum(),
            _ => 0,
        },
        Seg::Idx(i) => v.as_array_mut().and_then(|a| a.get_mut(*i)).map_or(0, |x| apply(x, rest, val)),
        Seg::Match(k, want) => {
            let Some(a) = v.as_array_mut() else { return 0 };
            let hit = |x: &Value| x.get(k).and_then(Value::as_str) == Some(want.as_str());
            let mut n = 0;
            for x in a.iter_mut().filter(|x| hit(x)) {
                n += apply(x, rest, val);
            }
            if n == 0 && !a.iter().any(hit) && matches!(rest.first(), Some(Seg::Key(_))) {
                let mut m = Map::new();
                m.insert(k.clone(), Value::String(want.clone()));
                let mut el = Value::Object(m);
                n = apply(&mut el, rest, val);
                if n > 0 {
                    a.push(el);
                }
            }
            n
        }
    }
}

/// Apply one rule to a resource value; returns nodes set.
pub fn apply_rule(v: &mut Value, kind: &str, segs: &[Seg], val: &Value) -> usize {
    if segs.first() == Some(&Seg::Pod) {
        let Some(p) = pod_path(kind) else { return 0 };
        let Some(spec) = v.pointer_mut(p) else { return 0 };
        apply(spec, &segs[1..], val)
    } else {
        apply(v, segs, val)
    }
}

/// How many enabled rules change this resource.
pub fn applied_rules(value: &Value, kind: &str, rules: &[Rule]) -> usize {
    rules
        .iter()
        .filter(|r| r.enabled)
        .filter_map(|r| r.parsed().ok())
        .filter(|(segs, val)| apply_rule(&mut value.clone(), kind, segs, val) > 0)
        .count()
}

/// The value with all enabled, valid rules applied, then re-redacted so a
/// rule can never put a credential-looking value into an export.
pub fn effective(value: &Value, kind: &str, rules: &[Rule]) -> Value {
    let mut v = value.clone();
    let mut any = false;
    for r in rules.iter().filter(|r| r.enabled) {
        if let Ok((segs, val)) = r.parsed()
            && apply_rule(&mut v, kind, &segs, &val) > 0
        {
            any = true;
        }
    }
    if any {
        v["kind"] = Value::String(kind.into());
        sanitize::redact(&mut v, kind);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn path_roundtrip() {
        let p = "$pod.containers[*].env[name=A].value";
        assert_eq!(to_path(&parse(p).unwrap()), p);
        let p = "$pod.nodeSelector.\"eks.amazonaws.com/nodegroup\"";
        assert_eq!(to_path(&parse(p).unwrap()), p);
    }

    #[test]
    fn applies_across_kinds_and_creates() {
        let rule = Rule { enabled: true, path: "$pod.containers[*].env[name=REGION].value".into(), value: "eu-1".into() };
        let mut d = json!({"spec":{"template":{"spec":{"containers":[{"name":"a","env":[{"name":"REGION","value":"x"}]},{"name":"b"}]}}}});
        let mut c = json!({"spec":{"jobTemplate":{"spec":{"template":{"spec":{"containers":[{"name":"a","env":[]}]}}}}}});
        let out_d = effective(&d, "Deployment", std::slice::from_ref(&rule));
        let out_c = effective(&c, "CronJob", &[rule]);
        assert_eq!(out_d.pointer("/spec/template/spec/containers/0/env/0/value").unwrap(), "eu-1");
        d = out_d;
        assert!(d.pointer("/spec/template/spec/containers/1/env").is_none()); // no env list to add to
        assert_eq!(out_c.pointer("/spec/jobTemplate/spec/template/spec/containers/0/env/0/value").unwrap(), "eu-1");
        c = out_c;
        let _ = c;
    }

    #[test]
    fn sensitive_values_still_redacted() {
        let rule = Rule { enabled: true, path: "$pod.containers[*].env[name=DB_PASSWORD].value".into(), value: "hunter2".into() };
        let d = json!({"spec":{"template":{"spec":{"containers":[{"name":"a","env":[{"name":"DB_PASSWORD","value":"x"}]}]}}}});
        let out = effective(&d, "Deployment", &[rule]);
        assert!(!out.to_string().contains("hunter2"));
    }
}
