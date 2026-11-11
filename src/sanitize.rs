//! Cleaning of fetched objects: strip cluster-managed fields, redact anything
//! that looks like a credential, and collect references to Secrets.

use serde_json::Value;
use std::collections::BTreeSet;

pub const REDACTED: &str = "<REDACTED>";

pub fn sensitive_name(name: &str) -> bool {
    let n = name.to_lowercase();
    ["secret", "password", "passwd", "token", "apikey", "api_key", "api-key",
     "private", "credential", "auth", "cert", "pwd"]
        .iter()
        .any(|k| n.contains(k))
        || n.ends_with("key")
        || n.ends_with("_key")
}

/// Stricter than `sensitive_name`: used inside free text (args, config files,
/// URLs) where words like "auth" or "cert" are common and harmless.
fn credential_key(k: &str) -> bool {
    let k = k.trim().trim_start_matches('-').trim_matches(|c| c == '"' || c == '\'').to_lowercase();
    !k.is_empty()
        && k.len() < 80
        && ["password", "passwd", "secret", "token", "apikey", "api_key", "api-key", "credential", "private_key", "private-key", "pwd"]
            .iter()
            .any(|w| k.contains(w))
}

/// Redacts `scheme://user:pass@host`, `--password=x` / `password=x` tokens and
/// `password: x` lines inside a larger string. Returns None if nothing changed.
pub fn scrub_text(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut changed = false;
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        // whole-line "key: value" / "key=value"
        let mut line_s = line.to_string();
        if let Some(pos) = line.find(['=', ':']) {
            let rest = line[pos + 1..].trim();
            let is_url = line[pos..].starts_with("://");
            if !is_url && credential_key(&line[..pos]) && !rest.is_empty() && rest != REDACTED {
                let sp = if line[pos + 1..].starts_with(' ') { " " } else { "" };
                line_s = format!("{}{}{}", &line[..=pos], sp, REDACTED);
                changed = true;
            }
        }
        // tokens: --password=x, user:pass@host
        let mut toks = vec![];
        for tok in line_s.split(' ') {
            let mut t = tok.to_string();
            if let Some(scheme) = t.find("://") {
                let auth_start = scheme + 3;
                let auth_end = t[auth_start..].find('/').map_or(t.len(), |e| auth_start + e);
                if let Some(at) = t[auth_start..auth_end].rfind('@') {
                    let ui = &t[auth_start..auth_start + at];
                    if let Some(colon) = ui.find(':')
                        && &ui[colon + 1..] != REDACTED
                    {
                        t = format!("{}{}:{}{}", &t[..auth_start], &ui[..colon], REDACTED, &t[auth_start + at..]);
                        changed = true;
                    }
                }
            } else if let Some((k, v)) = t.split_once('=')
                && credential_key(k)
                && !v.is_empty()
                && v != REDACTED
            {
                t = format!("{k}={REDACTED}");
                changed = true;
            }
            toks.push(t);
        }
        out.push_str(&toks.join(" "));
    }
    changed.then_some(out)
}

fn sensitive_value(v: &str) -> bool {
    v.contains("-----BEGIN")
}

/// Remove server-populated fields so the object can be applied elsewhere.
pub fn strip_cluster_fields(obj: &mut Value, kind: &str) {
    if let Some(meta) = obj.get_mut("metadata").and_then(Value::as_object_mut) {
        for k in ["uid", "resourceVersion", "creationTimestamp", "generation",
                  "managedFields", "selfLink", "ownerReferences", "deletionTimestamp",
                  "deletionGracePeriodSeconds"] {
            meta.remove(k);
        }
        if let Some(a) = meta.get_mut("annotations").and_then(Value::as_object_mut) {
            // last-applied-configuration is a verbatim copy of the manifest.
            a.remove("kubectl.kubernetes.io/last-applied-configuration");
            a.remove("deployment.kubernetes.io/revision");
            a.retain(|k, _| !k.starts_with("pv.kubernetes.io/"));
            if a.is_empty() {
                meta.remove("annotations");
            }
        }
    }
    obj.as_object_mut().map(|o| o.remove("status"));
    match kind {
        "Service" => {
            if let Some(s) = obj.get_mut("spec").and_then(Value::as_object_mut) {
                s.remove("clusterIP");
                s.remove("clusterIPs");
            }
        }
        "PersistentVolumeClaim" => {
            if let Some(s) = obj.get_mut("spec").and_then(Value::as_object_mut) {
                s.remove("volumeName");
            }
        }
        "ServiceAccount" => {
            // Auto-generated token Secret references.
            obj.as_object_mut().map(|o| o.remove("secrets"));
        }
        _ => {}
    }
    prune_nulls(obj);
}

fn prune_nulls(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|_, x| !x.is_null());
            m.values_mut().for_each(prune_nulls);
        }
        Value::Array(a) => a.iter_mut().for_each(prune_nulls),
        _ => {}
    }
}

/// Redact credential-looking values and return warnings describing what was
/// redacted or which Secrets the object references.
pub fn redact(obj: &mut Value, kind: &str) -> Vec<String> {
    let mut warnings = BTreeSet::new();
    let mut refs = BTreeSet::new();

    if kind == "ConfigMap" {
        for field in ["data", "binaryData"] {
            if let Some(d) = obj.get_mut(field).and_then(Value::as_object_mut) {
                for (k, v) in d.iter_mut() {
                    if sensitive_name(k) || v.as_str().is_some_and(sensitive_value) {
                        *v = Value::String(REDACTED.into());
                        warnings.insert(format!("ConfigMap key '{k}' looked sensitive and was redacted"));
                    }
                }
            }
        }
    }
    walk(obj, &mut warnings, &mut refs);
    let mut out: Vec<String> = warnings.into_iter().collect();
    for r in refs {
        out.push(format!("References Secret '{r}' (not exported) - create it in the target namespace"));
    }
    out
}

fn walk(v: &mut Value, warnings: &mut BTreeSet<String>, refs: &mut BTreeSet<String>) {
    match v {
        Value::Object(m) => {
            // Env var with an inline literal value and a sensitive-looking name.
            let name = m.get("name").and_then(Value::as_str).map(str::to_owned);
            if let Some(n) = name.filter(|n| sensitive_name(n) && m.get("value").is_some_and(Value::is_string)) {
                m.insert("value".into(), Value::String(REDACTED.into()));
                warnings.insert(format!("Env var '{n}' looked sensitive and was redacted"));
            }
            for key in ["secretKeyRef", "secretRef"] {
                if let Some(n) = m.get(key).and_then(|r| r.get("name")).and_then(Value::as_str) {
                    refs.insert(n.to_string());
                }
            }
            if let Some(n) = m.get("secretName").and_then(Value::as_str) {
                refs.insert(n.to_string());
            }
            if let Some(n) = m.get("secret").and_then(|s| s.get("secretName")).and_then(Value::as_str) {
                refs.insert(n.to_string());
            }
            if let Some(Value::Array(a)) = m.get("imagePullSecrets") {
                for x in a {
                    if let Some(n) = x.get("name").and_then(Value::as_str) {
                        refs.insert(n.to_string());
                    }
                }
            }
            for x in m.values_mut() {
                walk(x, warnings, refs);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| walk(x, warnings, refs)),
        Value::String(s) if sensitive_value(s) => {
            *s = REDACTED.into();
            warnings.insert("A PEM/private-key block was redacted".into());
        }
        Value::String(s) => {
            if let Some(clean) = scrub_text(s) {
                *s = clean;
                warnings.insert("Credential-like text (password=..., user:pass@host) was redacted".into());
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scrubs_credentials_in_text() {
        let mut o = json!({"args":["--db.password=hunter2","--mode=fast"],
            "data":{"url":"postgres://app:s3cret@db:5432/x","note":"hello","cfg":"a: 1\napi_token: abc\nb: 2"}});
        redact(&mut o, "ConfigMap");
        let t = o.to_string();
        for bad in ["hunter2", "s3cret", "abc"] {
            assert!(!t.contains(bad), "{bad} leaked: {t}");
        }
        assert!(t.contains("--mode=fast") && t.contains("hello") && t.contains("app:<REDACTED>@db:5432"));
    }

    #[test]
    fn redacts_env_and_configmap_and_pem() {
        let mut o = json!({"kind":"Deployment","spec":{"containers":[{"env":[
            {"name":"DB_PASSWORD","value":"hunter2"},
            {"name":"MODE","value":"prod"},
            {"name":"X","valueFrom":{"secretKeyRef":{"name":"creds","key":"k"}}}]}]}});
        let w = redact(&mut o, "Deployment");
        let s = o.to_string();
        assert!(!s.contains("hunter2") && s.contains("prod"));
        assert!(w.iter().any(|x| x.contains("creds")));
        let mut c = json!({"data":{"api_token":"abc","name":"x","f":"-----BEGIN KEY-----"}});
        redact(&mut c, "ConfigMap");
        assert!(!c.to_string().contains("abc") && !c.to_string().contains("BEGIN"));
    }
}
