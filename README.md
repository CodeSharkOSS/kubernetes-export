# Kubernetes Namespace Exporter

A desktop tool (Rust + egui) that uses your local kubeconfig to:

- **Export** a namespace to YAML files, with a GUI to review, edit, share values across resources
  and make the namespace replaceable (`__NAMESPACE__` template + `deploy.sh`).
- **Map** a namespace: Ingress → Service → workload → ConfigMap/PVC/ServiceAccount/Secret links,
  with problems (selectors matching nothing, dangling references) highlighted.
- **Deploy** an exported folder one resource at a time: edit, dry-run, apply, health-check, remove.

```
cargo run              # GUI
cargo build --release  # binary: target/release/kubernetes-export
cargo test
```

Requires a Rust toolchain (edition 2024) and a kubeconfig; the current context is used.
Exporting needs `list` on the resource kinds below; Deploy needs the rights to apply them.

## What gets exported

ConfigMap, ServiceAccount, PersistentVolumeClaim, Service, Deployment, StatefulSet, DaemonSet,
Job, CronJob, Ingress, NetworkPolicy, HorizontalPodAutoscaler, PodDisruptionBudget, Role and
RoleBinding. Server-populated fields (uid, resourceVersion, status, clusterIP, ...) are stripped
so the files can be applied elsewhere. Each export also writes an ordered `deploy.sh`.

Two output modes:

- **Template** (default): files contain `__NAMESPACE__`; run `./deploy.sh <new-namespace>`.
- **Replace** (`--target-namespace`): the namespace is rewritten up front; run `./deploy.sh`.

## Shared values

A *shared value* sets one node in every resource that has it, e.g. a region or node selector.
In the GUI use "Find & apply to all"; on the CLI use `--set 'path=yaml'`. Path syntax:

```
$pod.containers[*].env[name=AWS_REGION].value   # $pod = pod spec of any workload
key  "quoted.key/with.dots"  [*]  [name=foo]  [0]
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
                          [--template | --target-namespace qa-copy] [--set '$pod.containers[*].env[name=AWS_REGION].value=eu-west-1']
kubernetes-export plan    --dir ./export [--namespace qa-copy]  # offline
kubernetes-export check   --dir ./export --namespace qa-copy   # read-only; prints the cluster URL
kubernetes-export deploy  --dir ./export --namespace qa-copy   # server-side DRY RUN (default)
kubernetes-export deploy  --dir ./export --namespace qa-copy --only configmap-cfg.yaml --apply \
                          --confirm-namespace qa-copy --confirm-cluster <url from check> \
                          [--create-namespace] [--wait-secs 60]
kubernetes-export health  --dir ./export --namespace qa-copy [--only Deployment/app]
kubernetes-export remove  --dir ./export --namespace qa-copy --file Deployment/app \
                          --confirm-namespace qa-copy --confirm-cluster <url>
```

### MCP server

`kubernetes-export mcp` speaks MCP over stdio and exposes the same operations as tools
(`k8s_list_namespaces`, `k8s_map`, `k8s_export`, `k8s_plan`, `k8s_check`, `k8s_deploy`,
`k8s_health`, `k8s_remove`). Tool arguments mirror the CLI flags with underscores
(`confirm_namespace`, `target_namespace`, `create_namespace`, `wait_secs`, ...); `kind` is a list. Example client config:

```json
{ "mcpServers": { "kubernetes-export": { "command": "/path/to/kubernetes-export", "args": ["mcp"] } } }
```

Guard rails for unattended use: everything except `deploy --apply` and `remove` is read-only
(or writes only to the output folder); deploy is a dry run unless `apply` is set; applying or
removing needs both `confirm_namespace` (== the namespace) and `confirm_cluster` (== the cluster
URL reported by `check`) so a wrong kubeconfig context can't be hit by accident; `remove` only
deletes objects this tool created; unknown argument names are errors, so a typo can't silently
turn into a different action; Secrets are never exported, deployed or removed.

## Built with AI

This project was built with the assistance of AI (Claude, by Anthropic). The code has been reviewed
and tested, but as with any tool that touches a live cluster, read the dry-run output before applying.

## License

Licensed under the [Mozilla Public License 2.0](LICENSE).
