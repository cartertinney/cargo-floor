use crate::Args;
use crate::registry::CargoConfig;
use anyhow::{Context, Result, bail, ensure};
use cargo_metadata::{DependencyKind, Message, Metadata, Package, Target};
use semver::Version;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;
use toml_edit::{Array, DocumentMut, Item, Value, value};

#[derive(Clone)]
pub struct Dependency {
    pub metadata: cargo_metadata::Dependency,
    pub name: String,
    pub original_requirement: String,
    pub inherited: bool,
    target_key: Option<String>,
}

impl Dependency {
    pub fn kind(&self) -> &'static str {
        if self.metadata.kind == DependencyKind::Build {
            "build"
        } else {
            "normal"
        }
    }

    fn section(&self) -> &'static str {
        if self.metadata.kind == DependencyKind::Build {
            "build-dependencies"
        } else {
            "dependencies"
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Passed,
    ResolutionFailed,
    BuildFailed,
}

#[derive(Clone, Serialize)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: Version,
    pub source: Option<String>,
}

pub struct Build {
    pub outcome: Outcome,
    pub diagnostic: Option<String>,
    pub resolved: Vec<ResolvedPackage>,
    metadata: Option<Metadata>,
    compiled: BTreeSet<String>,
}

#[derive(Default)]
struct ActiveConditions {
    normal: BTreeSet<String>,
    build: BTreeSet<String>,
}

pub struct Project {
    pub name: String,
    pub manifest: PathBuf,
    pub dependencies: Vec<Dependency>,
    pub config: CargoConfig,
    pub rustc_version: String,
    pub host: String,
    pub target: String,
    pub overrides: bool,
    cwd: PathBuf,
    cargo: OsString,
    original_text: String,
    conditions: ActiveConditions,
    _scratch: TempDir,
    scratch_root: PathBuf,
    scratch_manifest: PathBuf,
    target_directory: PathBuf,
    base_document: DocumentMut,
}

impl Project {
    pub fn load(args: &Args) -> Result<Self> {
        let cwd = env::current_dir().context("find the current directory")?;
        let manifest = match &args.manifest_path {
            Some(path) => fs::canonicalize(path)
                .with_context(|| format!("find manifest {}", path.display()))?,
            None => cwd
                .ancestors()
                .map(|path| path.join("Cargo.toml"))
                .find(|path| path.is_file())
                .context("no Cargo.toml found; use --manifest-path")?,
        };
        let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        let output = Command::new(&cargo)
            .current_dir(&cwd)
            .args([
                "metadata",
                "--no-deps",
                "--format-version",
                "1",
                "--manifest-path",
            ])
            .arg(&manifest)
            .output()
            .context("run cargo metadata; ensure Cargo is installed")?;
        ensure!(
            output.status.success(),
            "read project metadata:\n{}",
            diagnostic(&output)
        );
        let metadata: Metadata =
            serde_json::from_slice(&output.stdout).context("parse cargo metadata")?;
        let selected = select_package(&metadata, &manifest, args.package.as_deref())?;
        ensure!(
            selected.targets.iter().any(is_library),
            "package {} has no library target; cargo floor checks libraries",
            selected.name
        );
        let manifest = selected.manifest_path.as_std_path().to_owned();
        let original_text = fs::read_to_string(&manifest).context("read selected manifest")?;
        let original_document = original_text
            .parse::<DocumentMut>()
            .context("parse selected manifest")?;
        let workspace_root = metadata.workspace_root.as_std_path();
        let workspace_manifest = workspace_root.join("Cargo.toml");
        let workspace_document = fs::read_to_string(&workspace_manifest)?
            .parse::<DocumentMut>()
            .context("parse workspace manifest")?;
        let dependencies = dependencies(selected, &original_document, &workspace_document)?;
        let config = CargoConfig::load(&cwd)?;
        let rustc = config
            .rustc
            .clone()
            .unwrap_or_else(|| OsString::from("rustc"));
        let output = Command::new(&rustc)
            .current_dir(&cwd)
            .arg("-vV")
            .output()
            .context("run rustc; ensure the selected Rust toolchain is installed")?;
        ensure!(
            output.status.success(),
            "read Rust toolchain:\n{}",
            diagnostic(&output)
        );
        let rustc_version = String::from_utf8(output.stdout).context("read rustc version")?;
        let host = rustc_version
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .context("rustc did not report its host target")?
            .to_owned();
        ensure!(
            args.target.is_some() || config.targets.len() <= 1,
            "multiple configured targets are not supported in one run; select one with --target"
        );
        let target = args
            .target
            .clone()
            .or_else(|| config.targets.first().cloned())
            .unwrap_or_else(|| host.clone());
        let explicit_target = args.target.is_some() || !config.targets.is_empty();
        let scratch = tempfile::Builder::new()
            .prefix("cargo-floor-")
            .tempdir()
            .context("create isolated probe directory")?;
        let scratch_root = scratch.path().join("source");
        let target_directory = scratch.path().join("target");
        let conditions = active_conditions(
            &cargo,
            &cwd,
            scratch.path(),
            &dependencies,
            explicit_target.then_some(target.as_str()),
        )?;
        let excluded = [
            metadata.target_directory.as_std_path().to_owned(),
            scratch.path().to_owned(),
            config.cargo_home.clone().unwrap_or_default(),
        ];
        copy_tree(workspace_root, &scratch_root, &excluded)?;
        for package in &metadata.packages {
            let original = package.manifest_path.as_std_path();
            if let Ok(relative) = original.strip_prefix(workspace_root) {
                let path = scratch_root.join(relative);
                let mut document = fs::read_to_string(&path)?.parse::<DocumentMut>()?;
                isolate_library_dependencies(&mut document, has_build_script(package));
                rewrite_paths(
                    &mut document,
                    original.parent().context("manifest has no parent")?,
                    workspace_root,
                    &scratch_root,
                )?;
                fs::write(&path, document.to_string())?;
            }
        }
        let scratch_workspace_manifest = scratch_root.join("Cargo.toml");
        let mut scratch_workspace =
            fs::read_to_string(&scratch_workspace_manifest)?.parse::<DocumentMut>()?;
        rewrite_paths(
            &mut scratch_workspace,
            workspace_root,
            workspace_root,
            &scratch_root,
        )?;
        let relative_manifest = manifest
            .strip_prefix(workspace_root)
            .context("selected package is outside its workspace")?;
        prune_workspace(
            &mut scratch_workspace,
            relative_manifest
                .parent()
                .context("manifest has no parent")?,
        );
        fs::write(&scratch_workspace_manifest, scratch_workspace.to_string())?;
        let scratch_manifest = scratch_root.join(relative_manifest);
        let base_document = fs::read_to_string(&scratch_manifest)?.parse::<DocumentMut>()?;
        let overrides = config.overrides
            || workspace_document.get("patch").is_some()
            || workspace_document.get("replace").is_some();
        Ok(Self {
            name: selected.name.clone(),
            manifest,
            dependencies,
            config,
            rustc_version: rustc_version
                .lines()
                .next()
                .unwrap_or(&rustc_version)
                .to_owned(),
            host,
            target,
            overrides,
            cwd,
            cargo,
            original_text,
            conditions,
            _scratch: scratch,
            scratch_root,
            scratch_manifest,
            target_directory,
            base_document,
        })
    }

    pub fn active_package<'a>(
        &self,
        build: &'a Build,
        dependency: &Dependency,
    ) -> Option<&'a Package> {
        let metadata = build.metadata.as_ref()?;
        let root = metadata
            .packages
            .iter()
            .find(|package| package.manifest_path.as_std_path() == self.scratch_manifest)?;
        let node = metadata
            .resolve
            .as_ref()?
            .nodes
            .iter()
            .find(|node| node.id == root.id)?;
        let build_dependency = dependency.metadata.kind == DependencyKind::Build;
        if build_dependency && !has_build_script(root) {
            return None;
        }
        let conditions = if build_dependency {
            &self.conditions.build
        } else {
            &self.conditions.normal
        };
        if dependency
            .metadata
            .target
            .as_ref()
            .is_some_and(|platform| !conditions.contains(&platform.to_string()))
        {
            return None;
        }
        node.deps.iter().find_map(|edge| {
            if !edge.dep_kinds.iter().any(|kind| {
                kind.kind == dependency.metadata.kind && kind.target == dependency.metadata.target
            }) {
                return None;
            }
            let package = metadata.packages.iter().find(|package| {
                package.id == edge.pkg
                    && package.name == dependency.metadata.name
                    && build.compiled.contains(&package.id.to_string())
            })?;
            let import_name = if dependency.metadata.rename.is_some() {
                dependency.name.replace('-', "_")
            } else {
                package
                    .targets
                    .iter()
                    .find(|target| is_library(target))?
                    .name
                    .clone()
            };
            (edge.name == import_name).then_some(package)
        })
    }

    pub fn write_requirements(&self, requirements: &[Option<String>]) -> Result<()> {
        let mut document = self.base_document.clone();
        for (dependency, requirement) in self.dependencies.iter().zip(requirements) {
            if let Some(requirement) = requirement {
                set_requirement(&mut document, dependency, requirement)?;
            }
        }
        fs::write(&self.scratch_manifest, document.to_string())
            .context("write isolated probe manifest")?;
        match fs::remove_file(self.scratch_root.join("Cargo.lock")) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("reset isolated probe lockfile"),
        }
        Ok(())
    }

    pub fn build(&self, args: &Args) -> Result<Build> {
        let mut metadata_command = self.command("metadata", args);
        metadata_command.args(["--format-version", "1"]);
        let output = metadata_command
            .output()
            .context("run Cargo dependency resolution")?;
        if !output.status.success() {
            return Ok(Build {
                outcome: Outcome::ResolutionFailed,
                diagnostic: Some(diagnostic(&output)),
                resolved: Vec::new(),
                metadata: None,
                compiled: BTreeSet::new(),
            });
        }
        let metadata: Metadata =
            serde_json::from_slice(&output.stdout).context("parse resolved Cargo metadata")?;
        let mut command = self.command("check", args);
        command.args([
            "--lib",
            "--locked",
            "--message-format",
            "json",
            "--package",
            &self.name,
        ]);
        if let Some(target) = &args.target {
            command.args(["--target", target]);
        }
        let output = command
            .output()
            .context("compile the isolated library probe")?;
        let mut compiled = BTreeSet::new();
        let mut messages = String::new();
        for message in Message::parse_stream(output.stdout.as_slice()) {
            match message.context("parse Cargo compiler messages")? {
                Message::CompilerArtifact(artifact) => {
                    compiled.insert(artifact.package_id.to_string());
                }
                Message::CompilerMessage(message) => {
                    if let Some(rendered) = message.message.rendered {
                        messages.push_str(&rendered);
                    }
                }
                Message::TextLine(line) => {
                    messages.push_str(&line);
                    messages.push('\n');
                }
                _ => {}
            }
        }
        let outcome = if output.status.success() {
            Outcome::Passed
        } else {
            messages.push_str(&String::from_utf8_lossy(&output.stderr));
            Outcome::BuildFailed
        };
        let resolved = metadata
            .packages
            .iter()
            .filter(|package| compiled.contains(&package.id.to_string()))
            .map(|package| ResolvedPackage {
                name: package.name.clone(),
                version: package.version.clone(),
                source: package.source.as_ref().map(ToString::to_string),
            })
            .collect();
        Ok(Build {
            outcome,
            diagnostic: (outcome != Outcome::Passed).then_some(messages),
            resolved,
            metadata: Some(metadata),
            compiled,
        })
    }

    fn command(&self, subcommand: &str, args: &Args) -> Command {
        let mut command = Command::new(&self.cargo);
        command
            .current_dir(&self.cwd)
            .env("CARGO_TERM_COLOR", "never")
            .env("CARGO_TARGET_DIR", &self.target_directory)
            .env("CARGO_BUILD_BUILD_DIR", &self.target_directory)
            .env(
                "CARGO_RESOLVER_LOCKFILE_PATH",
                self.scratch_root.join("Cargo.lock"),
            )
            .arg(subcommand)
            .arg("--manifest-path")
            .arg(&self.scratch_manifest);
        if args.no_default_features {
            command.arg("--no-default-features");
        }
        if args.all_features {
            command.arg("--all-features");
        }
        if !args.features.is_empty() {
            command.arg("--features").arg(args.features.join(","));
        }
        command
    }

    pub fn apply(&self, requirements: &[Option<String>]) -> Result<bool> {
        let document = self
            .original_text
            .parse::<toml_edit::ImDocument<String>>()?;
        let mut edits = Vec::new();
        for (dependency, requirement) in self.dependencies.iter().zip(requirements) {
            if let Some(requirement) = requirement {
                if requirement != &dependency.original_requirement {
                    let entry = dependency_entry(document.as_item(), dependency)?;
                    let version = if entry.as_str().is_some() {
                        entry
                    } else {
                        entry.get("version").context("dependency has no version")?
                    };
                    let span = version
                        .span()
                        .context("missing original version location")?;
                    let original = self
                        .original_text
                        .get(span.clone())
                        .context("invalid version location")?;
                    let replacement = if original.starts_with('\'') {
                        format!("'{requirement}'")
                    } else {
                        Value::from(requirement.as_str()).to_string()
                    };
                    edits.push((span, replacement));
                }
            }
        }
        if edits.is_empty() {
            return Ok(false);
        }
        edits.sort_by_key(|(span, _)| std::cmp::Reverse(span.start));
        let mut updated = self.original_text.clone();
        let mut previous_start = updated.len();
        for (span, replacement) in edits {
            ensure!(span.end <= previous_start, "overlapping manifest edits");
            previous_start = span.start;
            updated.replace_range(span, &replacement);
        }
        ensure!(
            fs::read_to_string(&self.manifest)? == self.original_text,
            "manifest changed during discovery; refusing to overwrite concurrent edits"
        );
        let directory = self.manifest.parent().context("manifest has no parent")?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(directory).context("create atomic manifest update")?;
        std::io::Write::write_all(&mut temporary, updated.as_bytes())?;
        temporary
            .as_file()
            .set_permissions(fs::metadata(&self.manifest)?.permissions())?;
        temporary
            .persist(&self.manifest)
            .context("replace manifest with verified requirements")?;
        Ok(true)
    }
}

fn select_package<'a>(
    metadata: &'a Metadata,
    manifest: &Path,
    name: Option<&str>,
) -> Result<&'a Package> {
    let members: Vec<_> = metadata
        .packages
        .iter()
        .filter(|package| metadata.workspace_members.contains(&package.id))
        .collect();
    if let Some(name) = name {
        return members
            .into_iter()
            .find(|package| package.name == name)
            .with_context(|| format!("package {name} is not a workspace member"));
    }
    if let Some(package) = members
        .iter()
        .find(|package| package.manifest_path.as_std_path() == manifest)
    {
        return Ok(package);
    }
    let defaults: Vec<_> = members
        .iter()
        .filter(|package| metadata.workspace_default_members.contains(&package.id))
        .collect();
    if defaults.len() == 1 {
        return Ok(defaults[0]);
    }
    if members.len() == 1 {
        return Ok(members[0]);
    }
    bail!("workspace contains multiple packages; select a library with --package")
}

fn is_library(target: &Target) -> bool {
    target.kind.iter().any(|kind| {
        matches!(
            kind.to_string().as_str(),
            "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
        )
    })
}

fn has_build_script(package: &Package) -> bool {
    package.targets.iter().any(|target| {
        target
            .kind
            .iter()
            .any(|kind| kind.to_string() == "custom-build")
    })
}

fn dependencies(
    package: &Package,
    document: &DocumentMut,
    workspace: &DocumentMut,
) -> Result<Vec<Dependency>> {
    let mut dependencies = Vec::new();
    for metadata in &package.dependencies {
        if metadata.kind == DependencyKind::Development {
            continue;
        }
        let name = metadata
            .rename
            .clone()
            .unwrap_or_else(|| metadata.name.clone());
        let target_key = if let Some(platform) = &metadata.target {
            let targets = document
                .get("target")
                .and_then(Item::as_table_like)
                .context("missing target dependency table")?;
            Some(
                targets
                    .iter()
                    .find_map(|(key, _)| {
                        key.parse::<cargo_platform::Platform>()
                            .ok()
                            .filter(|parsed| parsed == platform)
                            .map(|_| key.to_owned())
                    })
                    .context("cannot locate target dependency declaration")?,
            )
        } else {
            None
        };
        let mut dependency = Dependency {
            metadata: metadata.clone(),
            name,
            original_requirement: String::new(),
            inherited: false,
            target_key,
        };
        let entry = dependency_entry(document.as_item(), &dependency)?;
        dependency.inherited = entry.get("workspace").and_then(Item::as_bool) == Some(true);
        let version_entry = if dependency.inherited {
            workspace
                .get("workspace")
                .and_then(|item| item.get("dependencies"))
                .and_then(|item| item.get(&dependency.name))
                .context("missing inherited dependency")?
        } else {
            entry
        };
        dependency.original_requirement = version_entry
            .as_str()
            .or_else(|| version_entry.get("version").and_then(Item::as_str))
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| metadata.req.to_string());
        dependencies.push(dependency);
    }
    Ok(dependencies)
}

fn dependency_entry<'a>(document: &'a Item, dependency: &Dependency) -> Result<&'a Item> {
    let root = if let Some(target) = &dependency.target_key {
        document
            .get("target")
            .and_then(|item| item.get(target))
            .context("missing target table")?
    } else {
        document
    };
    root.get(dependency.section())
        .and_then(|item| item.get(&dependency.name))
        .with_context(|| format!("missing declaration of {}", dependency.name))
}

fn set_requirement(
    document: &mut DocumentMut,
    dependency: &Dependency,
    requirement: &str,
) -> Result<()> {
    let root = if let Some(target) = &dependency.target_key {
        document
            .get_mut("target")
            .and_then(|item| item.get_mut(target))
            .context("missing target table")?
    } else {
        document.as_item_mut()
    };
    let entry = root
        .get_mut(dependency.section())
        .and_then(|item| item.get_mut(&dependency.name))
        .with_context(|| format!("missing declaration of {}", dependency.name))?;
    let entry = if entry.as_str().is_some() {
        entry
    } else {
        entry
            .get_mut("version")
            .context("dependency has no editable version requirement")?
    };
    replace_string(entry, requirement)
}

fn replace_string(item: &mut Item, replacement: &str) -> Result<()> {
    let value = item
        .as_value_mut()
        .filter(|value| value.is_str())
        .context("expected a string value")?;
    let decor = value.decor().clone();
    *value = Value::from(replacement);
    *value.decor_mut() = decor;
    Ok(())
}

fn active_conditions(
    cargo: &OsString,
    cwd: &Path,
    scratch: &Path,
    dependencies: &[Dependency],
    target: Option<&str>,
) -> Result<ActiveConditions> {
    let conditions: BTreeSet<_> = dependencies
        .iter()
        .filter_map(|dependency| dependency.metadata.target.as_ref().map(ToString::to_string))
        .collect();
    if conditions.is_empty() {
        return Ok(ActiveConditions::default());
    }
    let root = scratch.join("conditions");
    fs::create_dir_all(&root)?;
    let mut document = r#"[package]
name = "cargo-floor-conditions"
version = "0.0.0"
edition = "2021"
[lib]
path = "lib.rs"
[workspace]
"#
    .parse::<DocumentMut>()?;
    fs::write(root.join("lib.rs"), "")?;
    fs::write(root.join("build.rs"), "fn main() {}\n")?;
    let mut names = BTreeMap::new();
    // Unique packages let Cargo distinguish declarations even when real edges
    // share a package ID across normal/build roles or target conditions.
    for (index, condition) in conditions.iter().enumerate() {
        for (role, section) in [("normal", "dependencies"), ("build", "build-dependencies")] {
            let name = format!("cargo-floor-condition-{role}-{index}");
            let path = root.join(&name);
            fs::create_dir_all(&path)?;
            fs::write(
                path.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
                 [lib]\npath = \"lib.rs\"\n"
                ),
            )?;
            fs::write(path.join("lib.rs"), "")?;
            document["target"][condition][section][&name]["path"] = value(name.clone());
            names.insert(name.replace('-', "_"), condition.clone());
        }
    }
    let manifest = root.join("Cargo.toml");
    fs::write(&manifest, document.to_string())?;
    let mut active = ActiveConditions::default();
    for (role, selected) in [("normal", &mut active.normal), ("build", &mut active.build)] {
        let mut command = Command::new(cargo);
        command
            .current_dir(cwd)
            .env("CARGO_TERM_COLOR", "never")
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env("CARGO_BUILD_BUILD_DIR", root.join("target"))
            .env("CARGO_RESOLVER_LOCKFILE_PATH", root.join("Cargo.lock"))
            .args(["tree", "--manifest-path"])
            .arg(&manifest)
            .args([
                "--package",
                "cargo-floor-conditions",
                "--edges",
                role,
                "--depth",
                "1",
                "--prefix",
                "depth",
                "--format",
                "|{lib}",
                "--charset",
                "ascii",
            ]);
        if let Some(target) = target {
            command.args(["--target", target]);
        }
        let output = command
            .output()
            .context("query Cargo dependency conditions")?;
        ensure!(
            output.status.success(),
            "query {role} dependency conditions:\n{}",
            diagnostic(&output)
        );
        let output =
            String::from_utf8(output.stdout).context("read Cargo condition probe output")?;
        let mut root_seen = false;
        for line in output.lines().filter(|line| !line.is_empty()) {
            if line == "0|cargo_floor_conditions" {
                root_seen = true;
            } else {
                let name = line
                    .strip_prefix("1|")
                    .context("unexpected Cargo condition probe depth")?;
                let condition = names
                    .get(name)
                    .context("unexpected package in Cargo condition probe")?;
                selected.insert(condition.clone());
            }
        }
        ensure!(
            root_seen,
            "Cargo condition probe did not report its root package"
        );
    }
    Ok(active)
}

fn diagnostic(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    )
}

fn copy_tree(source: &Path, destination: &Path, excluded: &[PathBuf]) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("copy workspace to {}", destination.display()))?;
    for entry in fs::read_dir(source)
        .with_context(|| format!("read workspace directory {}", source.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name() == ".git" || excluded.contains(&path) {
            continue;
        }
        let destination = destination.join(entry.file_name());
        let metadata =
            fs::metadata(&path).with_context(|| format!("inspect {}", path.display()))?;
        if metadata.is_dir() {
            ensure!(
                !entry.file_type()?.is_symlink(),
                "directory symlinks are not supported in isolated probes: {}",
                path.display()
            );
            copy_tree(&path, &destination, excluded)?;
        } else if metadata.is_file() {
            fs::copy(&path, &destination).with_context(|| format!("copy {}", path.display()))?;
        } else {
            bail!("unsupported special file in workspace: {}", path.display());
        }
    }
    Ok(())
}

fn isolate_library_dependencies(document: &mut DocumentMut, build_script: bool) {
    let local_features: BTreeSet<_> = document
        .get("features")
        .and_then(Item::as_table_like)
        .map(|features| features.iter().map(|(name, _)| name.to_owned()).collect())
        .unwrap_or_default();
    let mut removed = BTreeSet::new();
    let sections = if build_script {
        &["dev-dependencies", "dev_dependencies"][..]
    } else {
        &[
            "dev-dependencies",
            "dev_dependencies",
            "build-dependencies",
            "build_dependencies",
        ][..]
    };
    strip_dependency_sections(document.as_item_mut(), sections, &mut removed);
    if let Some(targets) = document.get_mut("target").and_then(Item::as_table_like_mut) {
        for (_, target) in targets.iter_mut() {
            strip_dependency_sections(target, sections, &mut removed);
        }
    }
    let mut retained_tables = vec![document.as_item()];
    if let Some(targets) = document.get("target").and_then(Item::as_table_like) {
        retained_tables.extend(targets.iter().map(|(_, target)| target));
    }
    for table in retained_tables {
        for section in ["dependencies", "build-dependencies"] {
            if let Some(dependencies) = table.get(section).and_then(Item::as_table_like) {
                for (name, _) in dependencies.iter() {
                    removed.remove(name);
                }
            }
        }
    }
    if let Some(features) = document
        .get_mut("features")
        .and_then(Item::as_table_like_mut)
    {
        for (_, feature) in features.iter_mut() {
            if let Some(values) = feature.as_array_mut() {
                values.retain(|value| {
                    let Some(value) = value.as_str() else {
                        return true;
                    };
                    if local_features.contains(value) {
                        return true;
                    }
                    let name = value
                        .strip_prefix("dep:")
                        .unwrap_or(value)
                        .split('/')
                        .next()
                        .unwrap_or(value)
                        .trim_end_matches('?');
                    !removed.contains(name)
                });
            }
        }
    }
}

fn strip_dependency_sections(item: &mut Item, sections: &[&str], removed: &mut BTreeSet<String>) {
    if let Some(table) = item.as_table_like_mut() {
        for key in sections {
            if let Some(dependencies) = table.remove(key) {
                if let Some(dependencies) = dependencies.as_table_like() {
                    removed.extend(dependencies.iter().map(|(name, _)| name.to_owned()));
                }
            }
        }
    }
}

fn rewrite_paths(
    document: &mut DocumentMut,
    original_directory: &Path,
    original_root: &Path,
    scratch_root: &Path,
) -> Result<()> {
    rewrite_dependency_paths(
        document.as_item_mut(),
        original_directory,
        original_root,
        scratch_root,
    )?;
    if let Some(targets) = document.get_mut("target").and_then(Item::as_table_like_mut) {
        for (_, target) in targets.iter_mut() {
            rewrite_dependency_paths(target, original_directory, original_root, scratch_root)?;
        }
    }
    if let Some(workspace) = document.get_mut("workspace") {
        rewrite_dependency_paths(workspace, original_directory, original_root, scratch_root)?;
    }
    for key in ["patch", "replace"] {
        if let Some(table) = document.get_mut(key).and_then(Item::as_table_like_mut) {
            for (_, entries) in table.iter_mut() {
                if key == "replace" {
                    rewrite_path(entries, original_directory, original_root, scratch_root)?;
                } else if let Some(entries) = entries.as_table_like_mut() {
                    for (_, entry) in entries.iter_mut() {
                        rewrite_path(entry, original_directory, original_root, scratch_root)?;
                    }
                }
            }
        }
    }
    if let Some(workspace) = document
        .get_mut("package")
        .and_then(Item::as_table_like_mut)
        .and_then(|table| table.get_mut("workspace"))
    {
        let path = original_directory.join(
            workspace
                .as_str()
                .context("package.workspace must be a path")?,
        );
        let path = fs::canonicalize(path)?;
        let mapped = map_path(&path, original_root, scratch_root);
        replace_string(
            workspace,
            mapped.to_str().context("workspace path is not UTF-8")?,
        )?;
    }
    Ok(())
}

fn rewrite_dependency_paths(
    item: &mut Item,
    directory: &Path,
    root: &Path,
    scratch: &Path,
) -> Result<()> {
    for section in ["dependencies", "build-dependencies", "dev-dependencies"] {
        if let Some(dependencies) = item.get_mut(section).and_then(Item::as_table_like_mut) {
            for (_, dependency) in dependencies.iter_mut() {
                rewrite_path(dependency, directory, root, scratch)?;
            }
        }
    }
    Ok(())
}

fn rewrite_path(item: &mut Item, directory: &Path, root: &Path, scratch: &Path) -> Result<()> {
    if let Some(path) = item
        .as_table_like_mut()
        .and_then(|table| table.get_mut("path"))
    {
        let original = directory.join(path.as_str().context("dependency path must be a string")?);
        let original = fs::canonicalize(&original)
            .with_context(|| format!("find path dependency {}", original.display()))?;
        let mapped = map_path(&original, root, scratch);
        replace_string(
            path,
            mapped.to_str().context("dependency path is not UTF-8")?,
        )?;
    }
    Ok(())
}

fn map_path(path: &Path, original_root: &Path, scratch_root: &Path) -> PathBuf {
    match path.strip_prefix(original_root) {
        Ok(relative) => scratch_root.join(relative),
        Err(_) => path.to_owned(),
    }
}

fn prune_workspace(document: &mut DocumentMut, selected: &Path) {
    if selected != Path::new("") {
        let keys: Vec<_> = document.iter().map(|(key, _)| key.to_owned()).collect();
        for key in keys {
            if !matches!(key.as_str(), "workspace" | "patch" | "replace" | "profile") {
                document.remove(&key);
            }
        }
    }
    if let Some(workspace) = document
        .get_mut("workspace")
        .and_then(Item::as_table_like_mut)
    {
        let mut members = Array::new();
        members.push(if selected.as_os_str().is_empty() {
            ".".into()
        } else {
            selected.to_string_lossy().into_owned()
        });
        workspace.insert("members", value(members.clone()));
        workspace.insert("default-members", value(members));
    }
}
