# Kubernetes Namespace Exporter

A desktop tool (Rust + egui) that uses your local kubeconfig to:

- **Export** a namespace to YAML files, with a GUI to review, edit, share values across resources
  and make the namespace replaceable (`__NAMESPACE__` template + `deploy.sh`).
- **Map** a namespace: Ingress → Service → workload → ConfigMap/PVC/ServiceAccount/Secret links,
  with problems (selectors matching nothing, dangling references) highlighted.
- **Deploy** an exported folder one resource at a time: edit, dry-run, apply, health-check, remove.

```
cargo run
cargo test
```

## Secrets

- Secrets are never requested from the API, never exported, and never deployed.
- Credential-looking values (sensitive env names, PEM blocks, `password=...`, `user:pass@host`)
  are redacted as `<REDACTED>`. Redaction goes by names/patterns, so skim the preview.
- Deploy mode lists Secret *names* (metadata-only request; no data is returned) to show which
  required Secrets are missing in the target namespace.

## Safety in Deploy mode

Dry-run needs nothing. Deploy / Create namespace / Remove require ticking a confirmation naming
the cluster and namespace. Remove only works on resources this tool applied in the session.

## Use from scripts and AI agents

With no arguments the GUI starts. With a subcommand it runs headless and prints JSON
(errors are `{"error": "..."}` with exit code 1):

```
kubernetes-export namespaces
kubernetes-export map     --namespace qa
kubernetes-export export  --namespace qa --out ./export [--kind Deployment] [--exclude Service/x] \
                          [--set '$pod.containers[*].env[name=AWS_REGION].value=eu-west-1']
kubernetes-export plan    --dir ./export                       # offline
kubernetes-export check   --dir ./export --namespace qa-copy   # read-only; prints the cluster URL
kubernetes-export deploy  --dir ./export --namespace qa-copy   # server-side DRY RUN (default)
kubernetes-export deploy  --dir ./export --namespace qa-copy --only configmap-cfg.yaml --apply \
                          --confirm-namespace qa-copy --confirm-cluster <url from check>
kubernetes-export health  --dir ./export --namespace qa-copy
kubernetes-export remove  --dir ./export --namespace qa-copy --file Deployment/app \
                          --confirm-namespace qa-copy --confirm-cluster <url>
```

### MCP server

`kubernetes-export mcp` speaks MCP over stdio and exposes the same operations as tools
(`k8s_list_namespaces`, `k8s_map`, `k8s_export`, `k8s_plan`, `k8s_check`, `k8s_deploy`,
`k8s_health`, `k8s_remove`). Example client config:

```json
{ "mcpServers": { "kubernetes-export": { "command": "/path/to/kubernetes-export", "args": ["mcp"] } } }
```

Guard rails for unattended use: everything except `deploy --apply` and `remove` is read-only
(or writes only to the output folder); deploy is a dry run unless `apply` is set; applying or
removing needs both `confirm_namespace` (== the namespace) and `confirm_cluster` (== the cluster
URL reported by `check`) so a wrong kubeconfig context can't be hit by accident; `remove` only
deletes objects this tool created; unknown argument names are errors, so a typo can't silently
turn into a different action; Secrets are never exported, deployed or removed.
