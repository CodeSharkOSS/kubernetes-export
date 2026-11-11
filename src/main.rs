//! Kubernetes namespace exporter.
//!
//! Exports the namespaced resources of a namespace to YAML files using the
//! local kubeconfig. Secrets are NEVER requested from the API server, and
//! values that look sensitive in other resources are redacted.

mod app;
mod cli;
mod common;
mod deploy;
mod editor;
mod export;
mod folder;
mod graph;
mod highlight;
mod k8s;
mod map;
mod mcp;
mod ops;
mod report;
mod sanitize;
mod shared;
mod widgets;

use eframe::egui;

fn main() -> eframe::Result {
    use clap::Parser;
    if let Some(cmd) = cli::Cli::parse().command {
        std::process::exit(cli::run(cmd));
    }
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 760.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Kubernetes Namespace Exporter",
        opts,
        Box::new(|_cc| Ok(Box::new(app::App::new()))),
    )
}
