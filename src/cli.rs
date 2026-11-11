//! Command-line interface. With no subcommand the GUI starts; with one, the
//! operation runs headless and prints JSON on stdout (errors too, as
//! `{"error": ...}` with exit code 1), so scripts and agents can parse it.

use crate::ops::{self, DeployArgs, DirArgs, ExportArgs, HealthArgs, MapArgs, RemoveArgs};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "kubernetes-export", version, about = "Export, map and deploy Kubernetes namespaces (GUI when run with no subcommand)")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// List namespaces on the current kubeconfig context.
    Namespaces,
    /// Export a namespace to YAML files (Secrets are never exported).
    Export(ExportArgs),
    /// Connection graph of a namespace (who routes to / uses what) plus problems.
    Map(MapArgs),
    /// Offline: describe an exported folder (order, kinds, Secrets needed).
    Plan(DirArgs),
    /// Read-only cluster check for a folder: namespace exists? Secrets present?
    Check(DirArgs),
    /// Deploy a folder. Dry run unless --apply (which needs --confirm-namespace and --confirm-cluster).
    Deploy(DeployArgs),
    /// Live health of the resources in a folder.
    Health(HealthArgs),
    /// Remove one resource that this tool deployed.
    Remove(RemoveArgs),
    /// Run an MCP server on stdio so an AI agent can use all of the above as tools.
    Mcp,
}

pub fn run(cmd: Cmd) -> i32 {
    // The MCP server owns its own runtime, so it must not be nested in `block_on`.
    match cmd {
        Cmd::Mcp => crate::mcp::serve(),
        cmd => run_headless(cmd),
    }
}

fn run_headless(cmd: Cmd) -> i32 {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            println!("{}", serde_json::json!({"error": e.to_string()}));
            return 1;
        }
    };
    let result = rt.block_on(async {
        match cmd {
            Cmd::Namespaces => ops::list_namespaces().await,
            Cmd::Export(a) => ops::export(a).await,
            Cmd::Map(a) => ops::map(a).await,
            Cmd::Plan(a) => ops::plan(&a),
            Cmd::Check(a) => ops::check(a).await,
            Cmd::Deploy(a) => ops::deploy(a).await,
            Cmd::Health(a) => ops::health(a).await,
            Cmd::Remove(a) => ops::remove(a).await,
            Cmd::Mcp => Err("mcp is not a one-shot command".to_string()),
        }
    });
    match result {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            0
        }
        Err(e) => {
            println!("{}", serde_json::json!({"error": e}));
            1
        }
    }
}
