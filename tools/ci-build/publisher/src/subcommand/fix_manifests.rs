/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

//! Subcommand for fixing manifest dependency version numbers.
//!
//! Finds all of the version numbers for every crate in the repo crate path, and then
//! finds all references to the crates in that path and updates them to have the correct
//! version numbers in addition to the dependency path.

use crate::fs::Fs;
use crate::package::{discover_manifests, parse_version};
use crate::SDK_REPO_NAME;
use anyhow::{bail, Context, Result};
use clap::Parser;
use semver::Version;
use smithy_rs_tool_common::ci::running_in_ci;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use toml::value::Table;
use toml::Value;
use tracing::info;

mod validate;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Mode {
    Check,
    Execute,
}

#[derive(Parser, Debug)]
pub struct FixManifestsArgs {
    /// Path containing the manifests to fix. Manifests will be discovered recursively
    #[clap(long)]
    location: PathBuf,
    /// Checks manifests rather than fixing them
    #[clap(long)]
    check: bool,
    /// Disable expected version number validation. This should only be used
    /// when SDK crates are being generated with independent version numbers.
    #[clap(long)]
    disable_version_number_validation: bool,
}

pub async fn subcommand_fix_manifests(
    FixManifestsArgs {
        location,
        check,
        disable_version_number_validation,
    }: &FixManifestsArgs,
) -> Result<()> {
    let mode = match check {
        true => Mode::Check,
        false => Mode::Execute,
    };
    let manifest_paths = discover_manifests(location.into()).await?;
    let mut manifests = read_manifests(Fs::Real, manifest_paths).await?;
    let versions = package_versions(&manifests)?;

    validate::validate_before_fixes(&versions, *disable_version_number_validation)?;
    fix_manifests(Fs::Real, &versions, &mut manifests, mode).await?;
    validate::validate_after_fixes(location).await?;
    info!("Successfully fixed manifests!");
    Ok(())
}

struct Manifest {
    path: PathBuf,
    metadata: toml::Value,
}

async fn read_manifests(fs: Fs, manifest_paths: Vec<PathBuf>) -> Result<Vec<Manifest>> {
    let mut result = Vec::new();
    for path in manifest_paths {
        let contents = fs.read_file(&path).await?;
        let metadata = toml::from_slice(&contents)
            .with_context(|| format!("failed to load package manifest for {:?}", &path))?;
        result.push(Manifest { path, metadata });
    }
    Ok(result)
}

/// Returns a map of crate name to semver version number
fn package_versions(manifests: &[Manifest]) -> Result<BTreeMap<String, Version>> {
    let mut versions = BTreeMap::new();
    for manifest in manifests {
        // ignore workspace manifests
        let package = match manifest.metadata.get("package") {
            Some(package) => package,
            None => continue,
        };
        // ignore non-publishable crates
        if let Some(Value::Boolean(false)) = manifest
            .metadata
            .get("package")
            .expect("checked above")
            .get("publish")
        {
            continue;
        }
        let name = package
            .get("name")
            .and_then(|name| name.as_str())
            .ok_or_else(|| {
                anyhow::Error::msg(format!("{:?} is missing a package name", manifest.path))
            })?;
        let version = package
            .get("version")
            .and_then(|name| name.as_str())
            .ok_or_else(|| {
                anyhow::Error::msg(format!("{:?} is missing a package version", manifest.path))
            })?;
        let version = parse_version(&manifest.path, version)?;
        versions.insert(name.into(), version);
    }
    Ok(versions)
}

fn fix_dep_set(
    versions: &BTreeMap<String, Version>,
    key: &str,
    metadata: &mut toml::Value,
) -> Result<usize> {
    let mut changed = 0;
    if let Some(dependencies) = metadata.as_table_mut().unwrap().get_mut(key) {
        if let Some(dependencies) = dependencies.as_table_mut() {
            for (dep_name, dep) in dependencies.iter_mut() {
                changed += match dep.as_table_mut() {
                    None => {
                        if !dep.is_str() {
                            bail!("unexpected dependency (must be table or string): {:?}", dep)
                        }
                        0
                    }
                    Some(ref mut table) => update_dep(table, dep_name, versions)?,
                };
            }
        }
    }
    Ok(changed)
}

fn update_dep(
    table: &mut Table,
    dep_name: &str,
    versions: &BTreeMap<String, Version>,
) -> Result<usize> {
    if !table.contains_key("path") {
        return Ok(0);
    }
    let package_version = match versions.get(dep_name) {
        Some(version) => version.to_string(),
        None => bail!("version not found for crate {}", dep_name),
    };
    let previous_version = table.insert(
        "version".into(),
        toml::Value::String(package_version.to_string()),
    );
    match previous_version {
        None => Ok(1),
        Some(prev_version) if prev_version.as_str() == Some(&package_version) => Ok(0),
        Some(mismatched_version) => {
            tracing::warn!(expected = ?package_version, actual = ?mismatched_version, "version was set but it did not match");
            Ok(1)
        }
    }
}

fn fix_dep_sets(versions: &BTreeMap<String, Version>, metadata: &mut toml::Value) -> Result<usize> {
    let mut changed = fix_dep_set(versions, "dependencies", metadata)?;
    changed += fix_dep_set(versions, "dev-dependencies", metadata)?;
    changed += fix_dep_set(versions, "build-dependencies", metadata)?;
    Ok(changed)
}

fn is_example_manifest(manifest_path: impl AsRef<Path>) -> bool {
    // Examine parent directories until either `examples/` or `aws-sdk-rust/` is found
    let mut path = manifest_path.as_ref();
    while let Some(parent) = path.parent() {
        path = parent;
        if path.file_name() == Some(OsStr::new("examples")) {
            return true;
        } else if path.file_name() == Some(OsStr::new(SDK_REPO_NAME)) {
            break;
        }
    }
    false
}

fn conditionally_disallow_publish(
    manifest_path: &Path,
    metadata: &mut toml::Value,
) -> Result<bool> {
    let is_github_actions = running_in_ci();
    let is_example = is_example_manifest(manifest_path);

    // Safe-guard to prevent accidental publish to crates.io. Add some friction
    // to publishing from a local development machine by detecting that the tool
    // is not being run from CI, and disallow publish in that case. Also disallow
    // publishing of examples.
    if !is_github_actions || is_example {
        if let Some(package) = metadata.as_table_mut().unwrap().get_mut("package") {
            info!(
                "Detected {}. Disallowing publish for {:?}.",
                if is_example { "example" } else { "local build" },
                manifest_path,
            );
            package
                .as_table_mut()
                .unwrap()
                .insert("publish".into(), toml::Value::Boolean(false));
            return Ok(true);
        }
    }
    Ok(false)
}

async fn fix_manifests(
    fs: Fs,
    versions: &BTreeMap<String, Version>,
    manifests: &mut Vec<Manifest>,
    mode: Mode,
) -> Result<()> {
    for manifest in manifests {
        let package_changed =
            conditionally_disallow_publish(&manifest.path, &mut manifest.metadata)?;
        let dependencies_changed = fix_dep_sets(versions, &mut manifest.metadata)?;
        if package_changed || dependencies_changed > 0 {
            let contents =
                "# Code generated by software.amazon.smithy.rust.codegen.smithy-rs. DO NOT EDIT.\n"
                    .to_string()
                    + &toml::to_string(&manifest.metadata).with_context(|| {
                        format!("failed to serialize to toml for {:?}", manifest.path)
                    })?;
            match mode {
                Mode::Execute => {
                    fs.write_file(&manifest.path, contents.as_bytes()).await?;
                    info!(
                        "Changed {} dependencies in {:?}.",
                        dependencies_changed, manifest.path
                    );
                }
                Mode::Check => {
                    bail!(
                        "{manifest:?} contained invalid versions",
                        manifest = manifest.path
                    )
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fix_dep_sets() {
        let manifest = br#"
            [package]
            name = "test"
            version = "1.2.0-preview"

            [build-dependencies]
            build_something = "1.3"
            local_build_something = { path = "../local_build_something", version = "0.4.0-different" }

            [dev-dependencies]
            dev_something = "1.1"
            local_dev_something = { path = "../local_dev_something" }

            [dependencies]
            something = "1.0"
            local_something = { path = "../local_something" }
        "#;
        let metadata = toml::from_slice(manifest).unwrap();
        let mut manifest = Manifest {
            path: "test".into(),
            metadata,
        };
        let versions = vec![
            ("local_build_something", "0.2.0"),
            ("local_dev_something", "0.1.0"),
            ("local_something", "1.1.3"),
        ]
        .into_iter()
        .map(|e| (e.0.to_string(), Version::parse(e.1).unwrap()))
        .collect();

        fix_dep_sets(&versions, &mut manifest.metadata).expect("success");

        let actual_deps = &manifest.metadata["dependencies"];
        assert_eq!(
            "\
                something = \"1.0\"\n\
                \n\
                [local_something]\n\
                path = \"../local_something\"\n\
                version = \"1.1.3\"\n\
            ",
            actual_deps.to_string()
        );

        let actual_dev_deps = &manifest.metadata["dev-dependencies"];
        assert_eq!(
            "\
                dev_something = \"1.1\"\n\
                \n\
                [local_dev_something]\n\
                path = \"../local_dev_something\"\n\
                version = \"0.1.0\"\n\
            ",
            actual_dev_deps.to_string()
        );

        let actual_build_deps = &manifest.metadata["build-dependencies"];
        assert_eq!(
            "\
                build_something = \"1.3\"\n\
                \n\
                [local_build_something]\n\
                path = \"../local_build_something\"\n\
                version = \"0.2.0\"\n\
            ",
            actual_build_deps.to_string()
        );
    }

    #[test]
    fn test_is_example_manifest() {
        assert!(!is_example_manifest("aws-sdk-rust/sdk/s3/Cargo.toml"));
        assert!(!is_example_manifest(
            "aws-sdk-rust/sdk/aws-config/Cargo.toml"
        ));
        assert!(!is_example_manifest(
            "/path/to/aws-sdk-rust/sdk/aws-config/Cargo.toml"
        ));
        assert!(!is_example_manifest("sdk/aws-config/Cargo.toml"));
        assert!(is_example_manifest("examples/foo/Cargo.toml"));
        assert!(is_example_manifest("examples/foo/bar/Cargo.toml"));
        assert!(is_example_manifest(
            "aws-sdk-rust/examples/foo/bar/Cargo.toml"
        ));
    }
}
