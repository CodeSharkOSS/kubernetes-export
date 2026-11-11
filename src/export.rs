//! Namespace rewriting and writing of manifests to disk.

use crate::k8s::Item;
use anyhow::{Context, Result};
use serde_json::Value;
use std::fmt::Write as _;
use std::path::Path;

pub const PLACEHOLDER: &str = "__NAMESPACE__";

pub enum Mode {
    /// Replace the old namespace with a concrete new one.
    Replace(String),
    /// Replace the old namespace with `__NAMESPACE__`.
    Template,
}

/// Safe file name: the resource name may have been edited by the user, so it
/// must never be able to contain path separators or start with a dot.
pub fn file_name(item: &Item) -> String {
    let name: String = item
        .name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();
    format!("{}-{}.yaml", item.kind.to_lowercase(), name.trim_start_matches('.'))
}

/// A Kubernetes namespace / DNS-1123 label.
pub fn valid_ns(ns: &str) -> bool {
    !ns.is_empty()
        && ns.len() <= 63
        && ns.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !ns.starts_with('-')
        && !ns.ends_with('-')
}

/// Order in which kinds are applied so dependencies exist first.
fn apply_rank(kind: &str) -> u8 {
    match kind {
        "ServiceAccount" => 0,
        "ConfigMap" | "PersistentVolumeClaim" => 1,
        "Role" | "RoleBinding" => 2,
        "Service" => 3,
        "Deployment" | "StatefulSet" | "DaemonSet" | "Job" | "CronJob" => 4,
        _ => 5,
    }
}

/// Rewrite only genuine namespace references, never arbitrary strings that
/// happen to equal the namespace name (e.g. `SPRING_PROFILES_ACTIVE=dev)`:
///  * any `namespace` field (metadata, `RoleBinding` subjects, refs, ...)
///  * env vars whose name contains NAMESPACE (e.g. `POD_NAMESPACE`)
///  * in-cluster DNS names of the form `<svc>.<ns>.svc[...]`
fn rewrite(v: &mut Value, old: &str, new: &str) {
    match v {
        Value::String(s) => {
            if s.contains(".svc") {
                *s = s.replace(&format!(".{old}.svc"), &format!(".{new}.svc"));
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| rewrite(x, old, new)),
        Value::Object(m) => {
            let ns_env = m
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.to_uppercase().contains("NAMESPACE"));
            for (k, x) in m.iter_mut() {
                let is_ns_field = k == "namespace" || (ns_env && k == "value");
                if is_ns_field && x.as_str() == Some(old) {
                    *x = Value::String(new.into());
                } else {
                    rewrite(x, old, new);
                }
            }
        }
        _ => {}
    }
}

pub fn render(item: &Item, old_ns: &str, mode: &Mode, rules: &[crate::common::Rule]) -> String {
    let mut v = crate::common::effective(&item.value, item.kind, rules);
    let new = match mode {
        Mode::Replace(n) => Some(n.as_str()),
        Mode::Template => Some(PLACEHOLDER),
    };
    if let Some(new) = new {
        rewrite(&mut v, old_ns, new);
        if let Some(meta) = v.get_mut("metadata").and_then(Value::as_object_mut) {
            meta.insert("namespace".into(), Value::String(new.into()));
        }
    }
    serde_yaml_ng::to_string(&v).unwrap_or_else(|e| format!("# serialization error: {e}"))
}

pub struct FileResult {
    pub kind: &'static str,
    pub resource: String,
    pub file: String,
    pub bytes: usize,
    pub error: Option<String>,
    /// Things worth knowing about this file (edits, shared values, redactions...).
    pub notes: Vec<String>,
}

pub struct Report {
    pub dir: std::path::PathBuf,
    pub files: Vec<FileResult>,
    /// None = deploy.sh written OK.
    pub script_error: Option<String>,
    pub templated: bool,
    /// .yaml files already in the folder that are not part of this export.
    pub ignored: Vec<String>,
}

impl Report {
    pub fn failed(&self) -> usize {
        self.files.iter().filter(|f| f.error.is_some()).count() + usize::from(self.script_error.is_some())
    }
    pub fn ok(&self) -> usize {
        self.files.iter().filter(|f| f.error.is_none()).count()
    }
}

/// Writes every selected resource. A failure on one file doesn't stop the rest;
/// each outcome is recorded in the report.
pub fn write_all(
    items: &[Item],
    old_ns: &str,
    mode: &Mode,
    rules: &[crate::common::Rule],
    dir: &Path,
) -> Result<Report> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut files = vec![];
    for item in items.iter().filter(|i| i.selected) {
        let text = render(item, old_ns, mode, rules);
        let file = file_name(item);
        let mut notes = vec![];
        if item.edited {
            notes.push("edited by you".to_string());
        }
        let shared = crate::common::applied_rules(&item.value, item.kind, rules);
        if shared > 0 {
            notes.push(format!("{shared} shared value(s) applied"));
        }
        let red = text.matches(crate::sanitize::REDACTED).count();
        if red > 0 {
            notes.push(format!("{red} value(s) redacted"));
        }
        notes.extend(item.warnings.iter().filter(|w| w.starts_with("References Secret")).cloned());
        let error = std::fs::write(dir.join(&file), &text).err().map(|e| e.to_string());
        files.push(FileResult { kind: item.kind, resource: item.name.clone(), file, bytes: text.len(), error, notes });
    }
    if let Mode::Replace(ns) = mode {
        anyhow::ensure!(valid_ns(ns), "'{ns}' is not a valid namespace name");
    }
    // Explicit, dependency-ordered list: stale files left in the folder from an
    // earlier export are never applied.
    let mut ordered: Vec<&FileResult> = files.iter().filter(|f| f.error.is_none()).collect();
    ordered.sort_by_key(|f| apply_rank(f.kind));
    let list = ordered.iter().fold(String::new(), |mut acc, f| {
        let _ = writeln!(acc, "  \"{}\"", f.file);
        acc
    });
    let script = match mode {
        Mode::Template => format!(
            "#!/usr/bin/env bash\n# Usage: ./deploy.sh <new-namespace>\nset -euo pipefail\n\
             NS=\"${{1:?usage: $0 <new-namespace>}}\"\n\
             [[ \"$NS\" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] || {{ echo \"invalid namespace: $NS\" >&2; exit 1; }}\n\
             cd \"$(dirname \"$0\")\"\n\
             FILES=(\n{list})\n\
             kubectl get ns \"$NS\" >/dev/null 2>&1 || kubectl create ns \"$NS\"\n\
             for f in \"${{FILES[@]}}\"; do\n  sed \"s/{PLACEHOLDER}/$NS/g\" \"$f\" | kubectl apply -f -\ndone\n"
        ),
        Mode::Replace(ns) => format!(
            "#!/usr/bin/env bash\nset -euo pipefail\ncd \"$(dirname \"$0\")\"\n\
             FILES=(\n{list})\n\
             kubectl get ns \"{ns}\" >/dev/null 2>&1 || kubectl create ns \"{ns}\"\n\
             for f in \"${{FILES[@]}}\"; do\n  kubectl apply -f \"$f\"\ndone\n"
        ),
    };
    let sp = dir.join("deploy.sh");
    let script_error = (|| -> std::io::Result<()> {
        std::fs::write(&sp, script)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sp, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    })()
    .err()
    .map(|e| e.to_string());
    let written: std::collections::HashSet<&str> = files.iter().map(|f| f.file.as_str()).collect();
    let mut ignored: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        // exact, lower-case ".yaml": the same strict rule as `folder::safe_name`
        .filter(|n| n.ends_with(".yaml") && !written.contains(n.as_str()))
        .collect();
    ignored.sort();
    Ok(Report {
        dir: dir.to_path_buf(),
        files,
        script_error,
        templated: matches!(mode, Mode::Template),
        ignored,
    })
}

#[cfg(test)]
mod tests {
    use super::{rewrite, valid_ns};
    use serde_json::json;

    #[test]
    fn export_is_contained_and_explicit() {
        use super::{Mode, write_all};
        let dir = std::env::temp_dir().join(format!("k8s-export-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stale-old.yaml"), "x").unwrap();
        let items = vec![
            crate::k8s::Item::new("ConfigMap", "../../evil".into(), json!({"kind":"ConfigMap","metadata":{"name":"x"}}), vec![]),
            crate::k8s::Item::new("Deployment", "app".into(), json!({"kind":"Deployment","metadata":{"name":"app"}}), vec![]),
        ];
        let r = write_all(&items, "dev", &Mode::Template, &[], &dir).unwrap();
        assert_eq!(r.failed(), 0);
        assert!(!dir.parent().unwrap().join("evil.yaml").exists());
        assert_eq!(r.ignored, vec!["stale-old.yaml".to_string()]);
        let script = std::fs::read_to_string(dir.join("deploy.sh")).unwrap();
        assert!(script.contains("configmap-.._.._evil.yaml") || script.contains("configmap-"));
        assert!(!script.contains("stale-old") && script.contains("deployment-app.yaml"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn namespace_validation() {
        assert!(valid_ns("qa-copy") && valid_ns("a1"));
        for bad in ["", "-a", "a-", "A", "a b", "a;rm -rf /", "a/b", "../x"] {
            assert!(!valid_ns(bad), "{bad}");
        }
    }
    #[test]
    fn only_namespace_references() {
        let mut v = json!({"metadata":{"namespace":"dev"},
            "env":[{"name":"SPRING_PROFILES_ACTIVE","value":"dev"},
                   {"name":"POD_NAMESPACE","value":"dev"},
                   {"name":"URL","value":"http://db.dev.svc.cluster.local:5432"}],
            "subjects":[{"kind":"ServiceAccount","name":"dev","namespace":"dev"}]});
        rewrite(&mut v, "dev", "X");
        let s = v.to_string();
        assert!(s.contains(r#""value":"dev""#) && s.matches("dev").count() == 2, "{s}");
        assert!(s.contains("db.X.svc.cluster.local") && s.contains(r#""namespace":"X""#));
    }
}
