mod progress;
mod project;
mod registry;
mod requirements;
mod search;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, ValueEnum};
use progress::Progress;
use project::{Build, Outcome, Project, ResolvedPackage};
use registry::{Registry, ReleaseIndex};
use requirements::SearchWindow;
use search::Strategy;
use semver::Version;
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "cargo-floor",
    bin_name = "cargo floor",
    version,
    about = "Discover source-driven minimum dependency versions for a Rust library"
)]
struct Args {
    #[arg(long, value_name = "PATH")]
    manifest_path: Option<PathBuf>,
    #[arg(short = 'p', long, value_name = "NAME")]
    package: Option<String>,
    #[arg(long, value_delimiter = ',', value_name = "FEATURES")]
    features: Vec<String>,
    #[arg(long)]
    no_default_features: bool,
    #[arg(long)]
    all_features: bool,
    #[arg(long, value_name = "TRIPLE")]
    target: Option<String>,
    #[arg(long, value_enum, default_value_t = Format::Human)]
    format: Format,
    /// Suppress progress messages, but keep warnings, errors, and the final report.
    #[arg(short = 'q', long)]
    quiet: bool,
    /// Check every earlier eligible release instead of assuming monotonic compatibility.
    #[arg(long)]
    exhaustive: bool,
    /// Apply verified requirement changes; leave the original lockfile unchanged.
    #[arg(long)]
    apply: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Human,
    Json,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    package: String,
    manifest_path: PathBuf,
    status: &'static str,
    configuration: Configuration,
    dependencies: Vec<DependencyReport>,
    baseline: Vec<ResolvedPackage>,
    final_resolution: Vec<ResolvedPackage>,
    warnings: Vec<String>,
    applied: bool,
}

#[derive(Serialize)]
struct Configuration {
    features: Vec<String>,
    default_features: bool,
    all_features: bool,
    target: String,
    host: String,
    rustc: String,
    search_strategy: Strategy,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Verified,
    Inactive,
    Unsupported,
    Unverified,
}

#[derive(Serialize)]
struct DependencyReport {
    name: String,
    package: String,
    kind: &'static str,
    target: Option<String>,
    original_requirement: String,
    proposed_requirement: String,
    lowest_verified: Option<Version>,
    earlier_candidates_checked: Option<bool>,
    status: Status,
    reason: Option<String>,
    probes: Vec<Probe>,
}

#[derive(Serialize)]
struct Probe {
    version: Version,
    outcome: Outcome,
    diagnostic: Option<String>,
    resolved: Vec<ResolvedPackage>,
}

fn main() {
    let mut arguments: Vec<_> = std::env::args_os().collect();
    if arguments.get(1).is_some_and(|argument| argument == "floor") {
        arguments.remove(1);
    }
    let args = Args::parse_from(arguments);
    if let Err(error) = run(&args) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> Result<()> {
    let progress = Progress::new(args.quiet);
    let strategy = if args.exhaustive {
        Strategy::Exhaustive
    } else {
        Strategy::Binary
    };
    let project = {
        let _activity = progress.activity(format_args!("Preparing isolated library workspace"));
        Project::load(args)?
    };
    let baseline = project.build(args, &progress, "baseline library")?;
    ensure!(
        baseline.outcome == Outcome::Passed,
        "baseline library build failed; fix the current configuration before discovering floors:\n{}",
        baseline
            .diagnostic
            .as_deref()
            .unwrap_or("Cargo did not provide a diagnostic")
    );
    let mut reports = Vec::new();
    let mut windows = Vec::new();
    let mut registries = Vec::new();
    for dependency in &project.dependencies {
        let mut report = DependencyReport {
            name: dependency.name.clone(),
            package: dependency.metadata.name.clone(),
            kind: dependency.kind(),
            target: dependency.metadata.target.as_ref().map(ToString::to_string),
            original_requirement: dependency.original_requirement.clone(),
            proposed_requirement: dependency.original_requirement.clone(),
            lowest_verified: None,
            earlier_candidates_checked: None,
            status: Status::Unverified,
            reason: None,
            probes: Vec::new(),
        };
        let mut window = None;
        let mut registry = None;
        if let Some(resolved) = project.active_package(&baseline, dependency) {
            let unsupported = if dependency.metadata.path.is_some()
                || !resolved.source.as_ref().is_some_and(|source| {
                    let source = source.to_string();
                    source.starts_with("registry+") || source.starts_with("sparse+")
                }) {
                Some(
                    "path, Git, and patched dependencies cannot establish a registry release floor"
                        .to_owned(),
                )
            } else if dependency.inherited {
                Some("workspace-inherited requirements are shared by other packages and are not modified or probed in v1".to_owned())
            } else if project.overrides {
                Some("dependency overrides are present; remove [patch], [replace], or Cargo path overrides to verify published registry sources".to_owned())
            } else {
                None
            };
            if let Some(reason) = unsupported {
                report.status = Status::Unsupported;
                report.reason = Some(reason);
            } else {
                match SearchWindow::new(&dependency.original_requirement, &resolved.version) {
                    Ok(search) => {
                        let source = project
                            .config
                            .registry(dependency.metadata.registry.as_deref())?;
                        if let Registry::Unsupported(reason) = &source {
                            report.status = Status::Unsupported;
                            report.reason = Some(reason.clone());
                        } else {
                            window = Some(search);
                            registry = Some(source);
                        }
                    }
                    Err(error) => {
                        report.status = Status::Unsupported;
                        report.reason = Some(error.to_string());
                    }
                }
            }
        } else {
            report.status = Status::Inactive;
            report.reason = Some(
                "not built by the selected library features, target, or build-script configuration"
                    .to_owned(),
            );
        }
        reports.push(report);
        windows.push(window);
        registries.push(registry);
    }
    let expanded: Vec<_> = windows
        .iter()
        .map(|window| window.as_ref().map(SearchWindow::expanded))
        .collect();
    let mut releases = ReleaseIndex::new();
    for (index, dependency) in project.dependencies.iter().enumerate() {
        let (Some(window), Some(registry)) = (&windows[index], &registries[index]) else {
            continue;
        };
        let candidates: Vec<_> = {
            let _activity =
                progress.activity(format_args!("Fetching releases for {}", dependency.name));
            releases.versions(registry, &dependency.metadata.name, project.config.offline)?
        }
        .into_iter()
        .filter(|version| window.contains(version))
        .collect();
        let candidate_count = candidates.len();
        progress.message(format_args!(
            "Searching {} with {} search ({candidate_count} eligible candidates)",
            dependency.name,
            strategy.name()
        ));
        let anchor = project
            .active_package(&baseline, dependency)
            .and_then(|resolved| candidates.binary_search(&resolved.version).ok());
        let report = &mut reports[index];
        let result =
            search::find_floor(candidate_count, anchor, strategy, &progress, |position| {
                let candidate = &candidates[position];
                progress.message(format_args!(
                    "Testing {} {candidate} (candidate {}/{candidate_count})",
                    dependency.name,
                    position + 1
                ));
                let mut requirements = expanded.clone();
                requirements[index] = Some(format!("={candidate}"));
                project.write_requirements(&requirements)?;
                let label = format!("{} {candidate}", dependency.name);
                let build = project.build(args, &progress, &label)?;
                if build.outcome == Outcome::Passed {
                    verify_candidate(&project, &build, index, candidate)?;
                }
                let passed = build.outcome == Outcome::Passed;
                match build.outcome {
                    Outcome::Passed => {
                        progress.message(format_args!("{} {candidate} verified", dependency.name));
                    }
                    Outcome::ResolutionFailed => {
                        progress.message(format_args!(
                            "{} {candidate} rejected: dependency resolution failed",
                            dependency.name
                        ));
                    }
                    Outcome::BuildFailed => {
                        progress.message(format_args!(
                            "{} {candidate} rejected: build failed",
                            dependency.name
                        ));
                    }
                }
                report.probes.push(Probe {
                    version: candidate.clone(),
                    outcome: build.outcome,
                    diagnostic: build.diagnostic,
                    resolved: build.resolved,
                });
                Ok(passed)
            })?;
        if let Some(floor) = result.floor {
            let candidate = &candidates[floor];
            report.status = Status::Verified;
            report.lowest_verified = Some(candidate.clone());
            report.earlier_candidates_checked = Some(result.earlier_candidates_checked);
            report.proposed_requirement = window.proposed(candidate);
        } else {
            report.reason = Some(
                "no stable, non-yanked release in the search window passed; inspect probe diagnostics".to_owned(),
            );
        }
    }
    let proposed: Vec<_> = reports
        .iter()
        .map(|report| {
            (report.status == Status::Verified).then(|| report.proposed_requirement.clone())
        })
        .collect();
    for (index, report) in reports.iter_mut().enumerate() {
        let Some(version) = &report.lowest_verified else {
            continue;
        };
        let mut requirements = proposed.clone();
        requirements[index] = Some(format!("={version}"));
        project.write_requirements(&requirements)?;
        let label = format!("final witness for {} {version}", report.name);
        let build = project.build(args, &progress, &label)?;
        ensure!(
            build.outcome == Outcome::Passed,
            "final requirements invalidate the {} {version} witness; no manifest was changed:\n{}",
            report.name,
            build.diagnostic.as_deref().unwrap_or("no Cargo diagnostic")
        );
        verify_candidate(&project, &build, index, version)?;
        let witness = report
            .probes
            .iter_mut()
            .find(|probe| probe.outcome == Outcome::Passed && probe.version == *version)
            .context("missing successful candidate witness")?;
        witness.resolved = build.resolved;
    }
    project.write_requirements(&proposed)?;
    let final_build = project.build(args, &progress, "final proposed requirements")?;
    ensure!(
        final_build.outcome == Outcome::Passed,
        "proposed requirements do not build together; no manifest was changed:\n{}",
        final_build
            .diagnostic
            .as_deref()
            .unwrap_or("no Cargo diagnostic")
    );
    if args.apply
        && reports
            .iter()
            .any(|report| report.status == Status::Unverified)
    {
        bail!("cannot apply unverified active dependency floors; inspect a report-only run");
    }
    let applied = args.apply && project.apply(&proposed)?;
    let warnings: Vec<_> = reports
        .iter()
        .filter(|report| matches!(report.status, Status::Unsupported | Status::Unverified))
        .filter_map(|report| {
            report
                .reason
                .as_ref()
                .map(|reason| format!("{}: {reason}", report.name))
        })
        .collect();
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    let report = Report {
        schema_version: 1,
        package: project.name.clone(),
        manifest_path: project.manifest.clone(),
        status: if warnings.is_empty() {
            "verified"
        } else {
            "incomplete"
        },
        configuration: Configuration {
            features: args.features.clone(),
            default_features: !args.no_default_features,
            all_features: args.all_features,
            target: project.target.clone(),
            host: project.host.clone(),
            rustc: project.rustc_version.clone(),
            search_strategy: strategy,
        },
        dependencies: reports,
        baseline: baseline.resolved,
        final_resolution: final_build.resolved,
        warnings,
        applied,
    };
    print_report(&report, args.format)
}

fn verify_candidate(
    project: &Project,
    build: &Build,
    index: usize,
    version: &Version,
) -> Result<()> {
    let dependency = &project.dependencies[index];
    let actual = project.active_package(build, dependency).with_context(|| {
        format!(
            "Cargo did not build the pinned dependency {}",
            dependency.name
        )
    })?;
    ensure!(
        actual.version == *version && actual.name == dependency.metadata.name,
        "Cargo did not select {} exactly at {version}",
        dependency.name
    );
    Ok(())
}

fn print_report(report: &Report, format: Format) -> Result<()> {
    let mut stdout = io::stdout().lock();
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(&mut stdout, report)?;
            writeln!(stdout)?;
        }
        Format::Human => {
            writeln!(
                stdout,
                "Dependency floors for {} ({})",
                report.package, report.status
            )?;
            writeln!(
                stdout,
                "{}; target {}",
                report.configuration.rustc, report.configuration.target
            )?;
            writeln!(
                stdout,
                "Search: {}",
                report.configuration.search_strategy.name()
            )?;
            for dependency in &report.dependencies {
                if dependency.lowest_verified.is_some() {
                    writeln!(
                        stdout,
                        "  {} [{}]: {} -> {}",
                        dependency.name,
                        dependency.kind,
                        dependency.original_requirement,
                        dependency.proposed_requirement
                    )?;
                } else {
                    writeln!(
                        stdout,
                        "  {} [{}]: {}",
                        dependency.name,
                        dependency.kind,
                        dependency.reason.as_deref().unwrap_or("unverified")
                    )?;
                }
            }
            if report
                .dependencies
                .iter()
                .any(|dependency| dependency.earlier_candidates_checked == Some(false))
            {
                writeln!(
                    stdout,
                    "\nBinary search assumes monotonic compatibility and may miss earlier working releases. \
                     Use --exhaustive to check every earlier eligible candidate."
                )?;
            }
            writeln!(
                stdout,
                "\nLowest verified for this configuration, not every allowed version combination."
            )?;
            if report.applied {
                writeln!(
                    stdout,
                    "Updated dependency requirements; Cargo.lock was not changed."
                )?;
            } else {
                writeln!(
                    stdout,
                    "No project files changed. Use --apply to edit verified requirements."
                )?;
            }
        }
    }
    Ok(())
}
