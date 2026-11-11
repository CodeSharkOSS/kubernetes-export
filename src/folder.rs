//! Reading an exported folder: which files, in what order, what they are, and
//! what they need. Pure file/YAML logic with no GUI or cluster access, shared
//! by the Deploy screen, the CLI and the MCP server.

use crate::{export, k8s, sanitize};
use serde_json::Value;
use std::path::Path;

pub struct ManifestFile {
    pub file: String,
    pub kind: &'static str,
    pub api_version: String,
    pub name: String,
    pub text: String,
    /// Why this file can never be deployed (Secrets, unsupported kinds).
    pub refused: Option<String>,
    pub warnings: Vec<String>,
    pub secret_refs: Vec<String>,
}

pub struct Folder {
    pub files: Vec<ManifestFile>,
    pub templated: bool,
    /// Namespace found in the files (when not templated).
    pub detected_ns: Option<String>,
    /// Problems reading individual files.
    pub errors: Vec<String>,
}

impl Folder {
    pub fn secrets_needed(&self) -> Vec<String> {
        let mut s: Vec<String> = self.files.iter().flat_map(|f| f.secret_refs.clone()).collect();
        s.sort();
        s.dedup();
        s
    }
}

/// Warnings and referenced Secret names for a manifest.
pub fn analyze(v: &Value, kind: &str, text: &str) -> (Vec<String>, Vec<String>) {
    let mut warnings = vec![];
    let mut secret_refs = vec![];
    let mut probe = v.clone();
    for w in sanitize::redact(&mut probe, kind) {
        if let Some(n) = w.strip_prefix("References Secret '").and_then(|r| r.split('\'').next()) {
            secret_refs.push(n.to_string());
        }
    }
    if text.contains(sanitize::REDACTED) {
        warnings.push("Contains <REDACTED> placeholders - the workload would receive that literal text. Replace them before deploying.".into());
    }
    // redact() only changes things that look like literal credentials
    let mut again = v.clone();
    sanitize::redact(&mut again, kind);
    if again != *v && !text.contains(sanitize::REDACTED) {
        warnings.push("Contains credential-like literal values. Prefer referencing a Secret instead of putting them in the file.".into());
    }
    (warnings, secret_refs)
}

/// A file name that stays inside the folder. The `.yaml` suffix is deliberately
/// case-sensitive: it is the strict allowlist for names this tool writes and reads.
fn safe_name(n: &str) -> bool {
    n.ends_with(".yaml") && !n.starts_with('.') && !n.contains(['/', '\\']) && !n.contains("..")
}

/// Read the files of an export, in `deploy.sh` order when present.
pub fn read(dir: &Path) -> Result<Folder, String> {
    let mut names: Vec<String> = vec![];
    let mut errors = vec![];
    if let Ok(script) = std::fs::read_to_string(dir.join("deploy.sh"))
        && let Some(start) = script.find("FILES=(")
        && let Some(end) = script[start..].find("\n)")
    {
        for l in script[start + 7..start + end].lines() {
            let n = l.trim().trim_matches('"');
            if n.is_empty() {
                continue;
            }
            if safe_name(n) {
                names.push(n.to_string());
            } else {
                errors.push(format!("deploy.sh lists '{n}', which is not a plain .yaml file name: ignored"));
            }
        }
    }
    if names.is_empty() {
        let rd = std::fs::read_dir(dir).map_err(|e| format!("Can't read '{}': {e}", dir.display()))?;
        names = rd.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| safe_name(n)).collect();
        names.sort();
    }
    let mut files = vec![];
    let (mut templated, mut detected_ns) = (false, None);
    for file in names {
        let text = match std::fs::read_to_string(dir.join(&file)) {
            Ok(t) => t,
            Err(e) => {
                errors.push(format!("{file}: {e}"));
                continue;
            }
        };
        let v: Value = serde_yaml_ng::from_str(&text).unwrap_or(Value::Null);
        let kind_s = v["kind"].as_str().unwrap_or("?").to_string();
        let api_version = v["apiVersion"].as_str().unwrap_or("").to_string();
        let name = v["metadata"]["name"].as_str().unwrap_or(&file).to_string();
        let known = k8s::kind_static(&kind_s);
        let refused = if kind_s == "Secret" {
            Some("Secrets are never deployed by this tool".to_string())
        } else if known.is_none() || k8s::resource_for(&api_version, &kind_s).is_none() {
            Some(format!("unsupported resource ({api_version} {kind_s})"))
        } else {
            None
        };
        let (warnings, secret_refs) = analyze(&v, &kind_s, &text);
        if detected_ns.is_none()
            && let Some(n) = v["metadata"]["namespace"].as_str().filter(|n| *n != export::PLACEHOLDER)
        {
            detected_ns = Some(n.to_string());
        }
        templated |= text.contains(export::PLACEHOLDER);
        files.push(ManifestFile { file, kind: known.unwrap_or("?"), api_version, name, text, refused, warnings, secret_refs });
    }
    if files.is_empty() && errors.is_empty() {
        errors.push("No .yaml files found in that folder.".into());
    }
    Ok(Folder { files, templated, detected_ns, errors })
}

/// The manifest as it will be sent: placeholder substituted, namespace forced.
pub fn manifest(text: &str, ns: &str) -> Result<Value, String> {
    let mut v: Value = serde_yaml_ng::from_str(&text.replace(export::PLACEHOLDER, ns)).map_err(|e| format!("invalid YAML: {e}"))?;
    v["metadata"]["namespace"] = Value::String(ns.to_string());
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_listed_in_deploy_sh() {
        let dir = std::env::temp_dir().join(format!("k8s-folder-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ok.yaml"), "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: ok\n").unwrap();
        std::fs::write(dir.join("deploy.sh"), "FILES=(\n  \"ok.yaml\"\n  \"../../etc/passwd.yaml\"\n  \"sub/x.yaml\"\n)\n").unwrap();
        let f = read(&dir).unwrap();
        assert_eq!(f.files.len(), 1);
        assert_eq!(f.errors.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
