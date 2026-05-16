//! Emit Kubernetes CRD manifests for every Velocity custom resource.
//!
//! Usage:
//!   cargo run -p velocity-types --bin generate-crds [-- --out-dir <dir>]
//!
//! Default output dir is `<workspace>/crds`. The script overwrites all
//! `*.yaml` files in that directory.

use std::path::PathBuf;

use anyhow::{Context, Result};
use kube::CustomResourceExt;
use velocity_types::crds::{
    ApiKey, Application, ArchivePolicy, AuthStrategy, Domain, LogFilterPolicy, LogRoutingPolicy,
    Organisation, PurgeRequest, RoleBinding, SchemaDefinition,
};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut out_dir = PathBuf::from("crds");
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--out-dir" | "-o" => {
                out_dir = PathBuf::from(args.get(i + 1).context("--out-dir requires value")?);
                i += 2;
            }
            "--help" | "-h" => {
                #[allow(clippy::print_stdout)]
                {
                    println!("generate-crds [--out-dir DIR]");
                }
                return Ok(());
            }
            other => anyhow::bail!("unknown arg: {other}"),
        }
    }

    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("create dir {}", out_dir.display()))?;

    let manifests: [(&str, String); 11] = [
        ("organisation", serde_yaml::to_string(&Organisation::crd())?),
        ("application", serde_yaml::to_string(&Application::crd())?),
        ("domain", serde_yaml::to_string(&Domain::crd())?),
        ("schemadefinition", serde_yaml::to_string(&SchemaDefinition::crd())?),
        ("authstrategy", serde_yaml::to_string(&AuthStrategy::crd())?),
        ("rolebinding", serde_yaml::to_string(&RoleBinding::crd())?),
        ("apikey", serde_yaml::to_string(&ApiKey::crd())?),
        ("archivepolicy", serde_yaml::to_string(&ArchivePolicy::crd())?),
        ("logfilterpolicy", serde_yaml::to_string(&LogFilterPolicy::crd())?),
        ("logroutingpolicy", serde_yaml::to_string(&LogRoutingPolicy::crd())?),
        ("purgerequest", serde_yaml::to_string(&PurgeRequest::crd())?),
    ];

    for (name, yaml) in &manifests {
        let path = out_dir.join(format!("{name}.yaml"));
        std::fs::write(&path, yaml).with_context(|| format!("write {}", path.display()))?;
        // Path display is the CLI's primary output channel — allow here.
        #[allow(clippy::print_stdout)]
        {
            println!("wrote {}", path.display());
        }
    }

    Ok(())
}
