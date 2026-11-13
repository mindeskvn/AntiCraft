//! Build-time settings that must not live in the public repository.
//!
//! The Antigravity account sign-in (Customizations > Account) uses Antigravity's own installed-app
//! OAuth client. Its secret is Google's, not ours to publish, so the source does not hold it: it
//! comes from the environment variable below or, when that is unset, from the git-ignored file
//! `antigravity-client.local.env` next to this file (`NAME=value` lines). A build without it still
//! works; only that sign-in says this build cannot do it.

use std::path::Path;

const LOCAL_FILE: &str = "antigravity-client.local.env";
const NAMES: [&str; 1] = ["ANTICRAFT_ANTIGRAVITY_CLIENT_SECRET"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // A watched file that does not exist reruns this script, and rebuilds the crate, on every
    // build; so the file is watched only once it is there. After creating it, touch build.rs.
    let file = Path::new(LOCAL_FILE);
    if file.exists() {
        println!("cargo:rerun-if-changed={LOCAL_FILE}");
    }
    let local = std::fs::read_to_string(file).unwrap_or_default();
    for name in NAMES {
        println!("cargo:rerun-if-env-changed={name}");
        let from_env = std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let from_file = local.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == name).then(|| value.trim().trim_matches('"').to_owned())
        });
        if let Some(value) = from_env.or(from_file).filter(|v| !v.is_empty()) {
            println!("cargo:rustc-env={name}={value}");
        }
    }
}
