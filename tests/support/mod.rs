//! Real Cargo fixtures, not a mock Cargo executable or application hook.
//!
//! Each registry contains deterministic `<name>-<version>.crate` tar.gz archives
//! and plain JSON-lines `index/<Cargo crate path>` entries with archive SHA-256s.
//! An isolated CARGO_HOME replaces crates.io with this absolute local-registry.
//! HTTP scenarios explicitly opt into a loopback-only sparse server; no fixture
//! uses a public index, remote archive, or process-global environment mutation.
//! Projects, compiler scratch files, and shared per-fixture build caches live
//! under the repository's `target/floor-e2e`; fixture directories are removed
//! after each test. Nested builds use CARGO, falling back to Cargo's compile-time
//! executable path, and never fetch crates.io releases.

#![allow(dead_code)]

mod gate;
mod sparse;

#[allow(unused_imports)]
pub use gate::BuildGate;
#[allow(unused_imports)]
pub use sparse::{IndexFault, SparseRegistry};

use flate2::{Compression, GzBuilder};
use semver::{Version, VersionReq};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

pub struct Release {
    name: String,
    version: String,
    source: String,
    dependencies: Vec<(String, String)>,
    features: BTreeMap<String, Vec<String>>,
    yanked: bool,
    extra_manifest: String,
    files: BTreeMap<String, String>,
}

impl Release {
    pub fn new(name: &str, version: &str, source: &str) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            source: source.into(),
            dependencies: Vec::new(),
            features: BTreeMap::new(),
            yanked: false,
            extra_manifest: String::new(),
            files: BTreeMap::new(),
        }
    }

    pub fn dependency(mut self, name: &str, requirement: &str) -> Self {
        self.dependencies.push((name.into(), requirement.into()));
        self
    }

    pub fn feature(mut self, name: &str, members: &[&str]) -> Self {
        self.features.insert(
            name.into(),
            members.iter().map(|member| (*member).into()).collect(),
        );
        self
    }

    pub fn yanked(mut self) -> Self {
        self.yanked = true;
        self
    }

    pub fn manifest(mut self, tables: &str) -> Self {
        self.extra_manifest.push_str(tables);
        self
    }

    pub fn file(mut self, path: &str, contents: &str) -> Self {
        self.files.insert(path.into(), contents.into());
        self
    }
}

pub struct Fixture {
    root: PathBuf,
    pub project: PathBuf,
    registry: PathBuf,
    cargo_home: PathBuf,
    target: PathBuf,
    scratch: PathBuf,
    offline: bool,
}

impl Fixture {
    pub fn new(name: &str) -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/floor-e2e")
            .join(format!("{name}-{}-{timestamp}-{id}", std::process::id()));
        let fixture = Self {
            project: root.join("project"),
            registry: root.join("registry"),
            cargo_home: root.join("cargo-home"),
            target: root.join("build"),
            scratch: root.join("scratch"),
            offline: true,
            root,
        };
        for directory in [
            &fixture.project,
            &fixture.registry.join("index"),
            &fixture.cargo_home,
            &fixture.target,
            &fixture.scratch,
        ] {
            fs::create_dir_all(directory).unwrap();
        }
        fs::write(
            fixture.cargo_home.join("config.toml"),
            format!(
                "[source.crates-io]\nreplace-with = \"fixture\"\n\
                 [source.fixture]\nlocal-registry = {}\n\
                 [net]\noffline = true\n",
                quoted(fixture.registry.to_str().unwrap()),
            ),
        )
        .unwrap();
        fixture
    }

    pub fn publish(&self, release: Release) {
        let mut manifest = format!(
            "[package]\nname = {}\nversion = {}\nedition = \"2021\"\n",
            quoted(&release.name),
            quoted(&release.version),
        );
        if !release.dependencies.is_empty() {
            manifest.push_str("\n[dependencies]\n");
            for (name, requirement) in &release.dependencies {
                manifest.push_str(&format!("{} = {}\n", quoted(name), quoted(requirement)));
            }
        }
        if !release.features.is_empty() {
            manifest.push_str("\n[features]\n");
            for (name, members) in &release.features {
                manifest.push_str(&format!(
                    "{} = {}\n",
                    quoted(name),
                    serde_json::to_string(members).unwrap(),
                ));
            }
        }
        manifest.push('\n');
        manifest.push_str(&release.extra_manifest);
        let encoder = GzBuilder::new()
            .mtime(0)
            .write(Vec::new(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let prefix = format!("{}-{}", release.name, release.version);
        let mut files = release.files.clone();
        files.insert("Cargo.toml".into(), manifest);
        files.insert("src/lib.rs".into(), release.source.clone());
        for (path, contents) in &files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            header.set_cksum();
            archive
                .append_data(&mut header, format!("{prefix}/{path}"), contents.as_bytes())
                .unwrap();
        }
        let bytes = archive.into_inner().unwrap().finish().unwrap();
        let checksum = format!("{:x}", Sha256::digest(&bytes));
        fs::write(self.registry.join(format!("{prefix}.crate")), bytes).unwrap();
        let dependencies: Vec<Value> = release
            .dependencies
            .iter()
            .map(|(name, requirement)| {
                json!({
                    "name": name,
                    "req": requirement,
                    "features": [],
                    "optional": false,
                    "default_features": true,
                    "target": null,
                    "kind": "normal",
                    "registry": null,
                })
            })
            .collect();
        let index_path = self.registry.join("index").join(index_path(&release.name));
        fs::create_dir_all(index_path.parent().unwrap()).unwrap();
        let mut index = OpenOptions::new()
            .create(true)
            .append(true)
            .open(index_path)
            .unwrap();
        writeln!(
            index,
            "{}",
            json!({
                "name": release.name,
                "vers": release.version,
                "deps": dependencies,
                "cksum": checksum,
                "features": release.features,
                "yanked": release.yanked,
                "links": null,
            })
        )
        .unwrap();
    }

    pub fn yank(&self, name: &str, version: &str) {
        let path = self.registry.join("index").join(index_path(name));
        let entries = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| {
                let mut entry: Value = serde_json::from_str(line).unwrap();
                if entry["vers"] == version {
                    entry["yanked"] = json!(true);
                }
                format!("{entry}\n")
            })
            .collect::<String>();
        fs::write(path, entries).unwrap();
    }

    pub fn cargo_home_config(&self, contents: &str) {
        fs::write(self.cargo_home.join("config.toml"), contents).unwrap();
    }

    pub fn registry_path(&self) -> &Path {
        &self.registry
    }

    pub fn scratch_path(&self) -> &Path {
        &self.scratch
    }

    pub fn sparse_registry(&mut self) -> SparseRegistry {
        let server = SparseRegistry::new(self.registry.clone());
        self.offline = false;
        self.cargo_home_config(&format!(
            "[source.crates-io]\nreplace-with = \"fixture\"\n\
             [source.fixture]\nregistry = {}\n[net]\noffline = false\n",
            quoted(&server.index_url()),
        ));
        server
    }

    pub fn named_sparse_registry(&mut self, name: &str, server: &SparseRegistry) {
        self.offline = false;
        self.write(
            ".cargo/config.toml",
            &format!(
                "[registries.{name}]\nindex = {}\n",
                quoted(&server.index_url())
            ),
        );
    }

    pub fn directory_source(&self) {
        let directory = self.root.join("directory-source");
        fs::create_dir_all(&directory).unwrap();
        for entry in fs::read_dir(&self.registry).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "crate") {
                let bytes = fs::read(&path).unwrap();
                let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
                archive.unpack(&directory).unwrap();
                let prefix = path.file_stem().unwrap();
                fs::write(
                    directory.join(prefix).join(".cargo-checksum.json"),
                    json!({"files": {}, "package": format!("{:x}", Sha256::digest(&bytes))})
                        .to_string(),
                )
                .unwrap();
            }
        }
        self.cargo_home_config(&format!(
            "[source.crates-io]\nreplace-with = \"fixture\"\n\
             [source.fixture]\ndirectory = {}\n[net]\noffline = true\n",
            quoted(directory.to_str().unwrap()),
        ));
    }

    pub fn project(&self, name: &str, tables: &str, source: &str) {
        self.write(
            "Cargo.toml",
            &format!(
                "[package]\nname = {}\nversion = \"0.1.0\"\nedition = \"2021\"\n\n{tables}\n",
                quoted(name),
            ),
        );
        self.write("src/lib.rs", source);
    }

    pub fn write(&self, relative: &str, contents: &str) {
        let path = self.project.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    pub fn read(&self, relative: &str) -> Vec<u8> {
        fs::read(self.project.join(relative)).unwrap()
    }

    pub fn snapshot(&self, paths: &[&str]) -> Vec<(String, Vec<u8>)> {
        paths
            .iter()
            .map(|path| ((*path).into(), self.read(path)))
            .collect()
    }

    pub fn assert_unchanged(&self, snapshot: &[(String, Vec<u8>)]) {
        for (path, original) in snapshot {
            assert_eq!(self.read(path), *original, "{path} was modified");
        }
    }

    pub fn cargo(&self, args: &[&str]) -> Output {
        self.cargo_command(args)
            .output()
            .expect("run the real Cargo executable")
    }

    pub fn cargo_command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(cargo_executable());
        command.args(args);
        self.configure(&mut command);
        command
    }

    pub fn check(&self, args: &[&str]) {
        let mut command_args = vec!["check", "--lib"];
        if self.offline {
            command_args.push("--offline");
        }
        command_args.extend_from_slice(args);
        assert_success(&self.cargo(&command_args));
    }

    pub fn floor_raw(&self, args: &[&str]) -> Output {
        self.floor_command(args).output().expect("run cargo-floor")
    }

    pub fn floor_command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-floor"));
        command.args(args);
        self.configure(&mut command);
        command
    }

    pub fn floor(&self, args: &[&str]) -> Value {
        let mut command_args = vec!["floor", "--format", "json"];
        command_args.extend_from_slice(args);
        parse_report(&self.floor_raw(&command_args))
    }

    pub fn host_target(&self) -> String {
        let output = self.cargo(&["-vV"]);
        assert_success(&output);
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .expect("Cargo's verbose version includes its host triple")
            .to_owned()
    }

    /// Independently install the proposal in the original, unpruned manifest,
    /// then make real Cargo select and compile the claimed minimum. All source
    /// features, aliases, target tables, and dev declarations remain intact.
    pub fn assert_floor_witness(&self, report: &Value, entry: &Value, args: &[&str]) {
        self.assert_floor_witness_with(report, entry, args, |_| {});
    }

    pub fn assert_floor_witness_with(
        &self,
        report: &Value,
        entry: &Value,
        args: &[&str],
        configure: impl Fn(&mut Command),
    ) {
        let cargo = |args: &[&str]| {
            let mut command = self.cargo_command(args);
            configure(&mut command);
            command
                .output()
                .expect("run real Cargo for an independent witness")
        };
        let manifest = PathBuf::from(report["manifest_path"].as_str().unwrap());
        assert!(manifest.starts_with(&self.project));
        let _restore = RestoreFiles::new(&[manifest.clone(), self.project.join("Cargo.lock")]);
        let mut document = fs::read_to_string(&manifest)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        for proposal in report["dependencies"].as_array().unwrap() {
            if proposal["status"] != "verified" {
                continue;
            }
            let root = if let Some(target) = proposal["target"].as_str() {
                // Cargo canonicalizes cfg expressions in metadata.
                let targets = document["target"].as_table_like_mut().unwrap();
                let key = targets
                    .iter()
                    .find(|(key, _)| {
                        key.parse::<cargo_platform::Platform>().ok()
                            == target.parse::<cargo_platform::Platform>().ok()
                    })
                    .map(|(key, _)| key.to_owned())
                    .unwrap();
                targets.get_mut(&key).unwrap()
            } else {
                document.as_item_mut()
            };
            let section = if proposal["kind"] == "build" {
                "build-dependencies"
            } else {
                "dependencies"
            };
            let item = &mut root[section][proposal["name"].as_str().unwrap()];
            let requirement = proposal["proposed_requirement"].as_str().unwrap();
            if item.as_str().is_some() {
                *item = toml_edit::value(requirement);
            } else {
                item["version"] = toml_edit::value(requirement);
            }
        }
        fs::write(&manifest, document.to_string()).unwrap();
        let lockfile = self.project.join("Cargo.lock");
        if lockfile.exists() {
            fs::remove_file(lockfile).unwrap();
        }
        let package = entry["package"].as_str().unwrap();
        let minimum = entry["lowest_verified"].as_str().unwrap();
        assert_success(&cargo(&[
            "update",
            "--manifest-path",
            manifest.to_str().unwrap(),
            "-p",
            package,
            "--precise",
            minimum,
        ]));
        let mut check = vec![
            "check",
            "--manifest-path",
            manifest.to_str().unwrap(),
            "--locked",
            "--lib",
            "--message-format",
            "json",
        ];
        check.extend_from_slice(args);
        let compiled = cargo(&check);
        if !compiled.status.success() {
            let diagnostics = compiled
                .stdout
                .split(|byte| *byte == b'\n')
                .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
                .filter_map(|message| message["message"]["rendered"].as_str().map(str::to_owned))
                .collect::<String>();
            panic!(
                "reported floor {package} {minimum} does not compile under the ORIGINAL \
                 manifest configuration:\n{diagnostics}\n{}",
                String::from_utf8_lossy(&compiled.stderr),
            );
        }
        let mut metadata = vec![
            "metadata",
            "--manifest-path",
            manifest.to_str().unwrap(),
            "--locked",
            "--format-version",
            "1",
        ];
        for argument in args {
            metadata.push(if *argument == "--target" {
                "--filter-platform"
            } else {
                argument
            });
        }
        let output = cargo(&metadata);
        assert_success(&output);
        let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();
        let actual = metadata["packages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["name"] == package && candidate["version"] == minimum)
            .unwrap_or_else(|| panic!("Cargo did not resolve {package} {minimum}: {metadata:#}"));
        assert!(
            compiled.stdout.split(|byte| *byte == b'\n').any(|line| {
                serde_json::from_slice::<Value>(line).is_ok_and(|message| {
                    message["reason"] == "compiler-artifact"
                        && message["package_id"] == actual["id"]
                })
            }),
            "the original selected configuration did not compile {package} {minimum}",
        );
    }

    fn configure(&self, command: &mut Command) {
        for (name, _) in std::env::vars_os() {
            let key = name.to_string_lossy();
            if matches!(
                key.as_ref(),
                "RUSTFLAGS"
                    | "CARGO_ENCODED_RUSTFLAGS"
                    | "RUSTC"
                    | "RUSTC_WRAPPER"
                    | "RUSTC_WORKSPACE_WRAPPER"
                    | "CARGO_BUILD_TARGET"
                    | "CARGO_BUILD_RUSTFLAGS"
                    | "CARGO_HTTP_PROXY"
                    | "CARGO_HTTP_USER_AGENT"
                    | "HTTP_PROXY"
                    | "HTTPS_PROXY"
                    | "ALL_PROXY"
                    | "http_proxy"
                    | "https_proxy"
                    | "all_proxy"
            ) || key.starts_with("CARGO_SOURCE_")
                || key.starts_with("CARGO_REGISTRIES_")
                || (key.starts_with("CARGO_TARGET_") && key.ends_with("_RUSTFLAGS"))
            {
                command.env_remove(name);
            }
        }
        command
            .current_dir(&self.project)
            .env("CARGO", cargo_executable())
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TARGET_DIR", &self.target)
            .env(
                "CARGO_NET_OFFLINE",
                if self.offline { "true" } else { "false" },
            )
            .env("CARGO_NET_RETRY", "0")
            .env("CARGO_HTTP_TIMEOUT", "5")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("CARGO_BUILD_JOBS", "1")
            .env("CARGO_INCREMENTAL", "0")
            .env("CARGO_TERM_COLOR", "never")
            .env("TMPDIR", &self.scratch)
            .env("TMP", &self.scratch)
            .env("TEMP", &self.scratch);
    }
}

struct RestoreFiles(Vec<(PathBuf, Option<Vec<u8>>)>);

impl RestoreFiles {
    fn new(paths: &[PathBuf]) -> Self {
        Self(
            paths
                .iter()
                .map(|path| (path.clone(), fs::read(path).ok()))
                .collect(),
        )
    }
}

impl Drop for RestoreFiles {
    fn drop(&mut self) {
        for (path, bytes) in &self.0 {
            if let Some(bytes) = bytes {
                let _ = fs::write(path, bytes);
            } else {
                let _ = fs::remove_file(path);
            }
        }
    }
}

pub struct RunningFloor(Option<Child>);

impl RunningFloor {
    pub fn spawn(command: &mut Command) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        Self(Some(
            command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn cargo-floor"),
        ))
    }

    pub fn finish(mut self) -> Output {
        self.0.take().unwrap().wait_with_output().unwrap()
    }

    pub fn take_stderr(&mut self) -> ChildStderr {
        self.0.as_mut().unwrap().stderr.take().unwrap()
    }
}

impl Drop for RunningFloor {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            #[cfg(unix)]
            {
                // Nested Cargo and build-script processes share this group.
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{}", child.id())])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn cargo_executable() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| OsString::from(env!("CARGO")))
}

fn index_path(name: &str) -> PathBuf {
    let name = name.to_ascii_lowercase();
    match name.len() {
        1 => PathBuf::from("1").join(name),
        2 => PathBuf::from("2").join(name),
        3 => PathBuf::from("3").join(&name[..1]).join(name),
        _ => PathBuf::from(&name[..2]).join(&name[2..4]).join(name),
    }
}

pub fn quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}

pub fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed ({})\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

pub fn assert_failure(output: &Output, explanation: &str) {
    assert!(
        !output.status.success(),
        "{explanation}: unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).trim().is_empty(),
        "{explanation}: expected actionable stderr",
    );
    if let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) {
        assert!(
            !value["dependencies"].is_array(),
            "{explanation}: failure must not emit a successful discovery report",
        );
    }
}

pub fn parse_report(output: &Output) -> Value {
    assert_success(output);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout must be exactly one JSON report: {error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    });
    assert_eq!(report["schema_version"], 1);
    assert!(report["package"].is_string(), "{report:#}");
    assert!(report["applied"].is_boolean(), "{report:#}");
    assert!(report["configuration"].is_object(), "{report:#}");
    assert!(
        matches!(report["status"].as_str(), Some("verified" | "incomplete")),
        "{report:#}",
    );
    for dependency in report["dependencies"]
        .as_array()
        .expect("report.dependencies is an array")
    {
        assert!(dependency["name"].is_string(), "{dependency:#}");
        assert!(dependency["package"].is_string(), "{dependency:#}");
        assert!(
            matches!(dependency["kind"].as_str(), Some("normal" | "build")),
            "{dependency:#}",
        );
        assert!(
            dependency["original_requirement"].is_string(),
            "{dependency:#}"
        );
        assert!(
            dependency["proposed_requirement"].is_string(),
            "{dependency:#}"
        );
        assert!(
            dependency["lowest_verified"].is_null() || dependency["lowest_verified"].is_string(),
            "{dependency:#}",
        );
        assert!(
            matches!(
                dependency["status"].as_str(),
                Some("verified" | "inactive" | "unsupported" | "unverified")
            ),
            "{dependency:#}",
        );
        for probe in dependency["probes"]
            .as_array()
            .expect("dependency.probes is an array")
        {
            assert!(probe["version"].is_string(), "{probe:#}");
            assert!(
                matches!(
                    probe["outcome"].as_str(),
                    Some("passed" | "resolution_failed" | "build_failed")
                ),
                "{probe:#}",
            );
            if probe["outcome"] == "passed" {
                assert_resolved(
                    probe,
                    dependency["package"].as_str().unwrap(),
                    probe["version"].as_str().unwrap(),
                );
            }
        }
    }
    report
}

pub fn dependency<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|dependency| dependency["name"] == name)
        .unwrap_or_else(|| panic!("missing dependency {name}: {report:#}"))
}

pub fn verified<'a>(
    report: &'a Value,
    name: &str,
    package: &str,
    original: &str,
    minimum: &str,
) -> &'a Value {
    let dependency = dependency(report, name);
    assert_eq!(dependency["package"], package, "{dependency:#}");
    assert_eq!(
        dependency["original_requirement"], original,
        "{dependency:#}"
    );
    assert_eq!(dependency["status"], "verified", "{dependency:#}");
    assert_eq!(dependency["lowest_verified"], minimum, "{dependency:#}");
    assert!(
        !dependency["proposed_requirement"]
            .as_str()
            .unwrap()
            .is_empty(),
        "{dependency:#}",
    );
    let requirement =
        VersionReq::parse(dependency["proposed_requirement"].as_str().unwrap()).unwrap();
    let minimum = Version::parse(minimum).unwrap();
    assert!(
        requirement.matches(&minimum),
        "proposal must admit its claimed floor: {dependency:#}",
    );
    let predecessor = if minimum.patch > 0 {
        Some(Version::new(
            minimum.major,
            minimum.minor,
            minimum.patch - 1,
        ))
    } else if minimum.minor > 0 {
        Some(Version::new(minimum.major, minimum.minor - 1, 999))
    } else if minimum.major > 0 {
        Some(Version::new(minimum.major - 1, 999, 999))
    } else {
        None
    };
    for below in predecessor.into_iter().chain(
        dependency["probes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|probe| Version::parse(probe["version"].as_str().unwrap()).unwrap())
            .filter(|version| version < &minimum),
    ) {
        assert!(
            !requirement.matches(&below),
            "proposal admits below-floor release {below}: {dependency:#}",
        );
    }
    probe(dependency, &minimum.to_string(), "passed");
    dependency
}

pub fn dependency_kind<'a>(report: &'a Value, name: &str, kind: &str) -> &'a Value {
    report["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == name && entry["kind"] == kind)
        .unwrap_or_else(|| panic!("missing {kind} dependency {name}: {report:#}"))
}

pub fn assert_unsupported(report: &Value, name: &str, original: &str) {
    let entry = dependency(report, name);
    assert_eq!(report["status"], "incomplete", "{report:#}");
    assert_eq!(entry["status"], "unsupported", "{entry:#}");
    assert_eq!(entry["original_requirement"], original, "{entry:#}");
    assert_eq!(entry["proposed_requirement"], original, "{entry:#}");
    assert!(entry["lowest_verified"].is_null(), "{entry:#}");
    assert!(entry["probes"].as_array().unwrap().is_empty(), "{entry:#}");
    assert!(
        !entry["reason"].as_str().unwrap().trim().is_empty(),
        "{entry:#}",
    );
    assert!(
        !report["warnings"].as_array().unwrap().is_empty(),
        "{report:#}"
    );
}

pub fn probe<'a>(dependency: &'a Value, version: &str, outcome: &str) -> &'a Value {
    dependency["probes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|probe| probe["version"] == version && probe["outcome"] == outcome)
        .unwrap_or_else(|| panic!("missing {outcome} probe of version {version}: {dependency:#}"))
}

pub fn assert_resolved(probe: &Value, name: &str, version: &str) {
    assert!(
        probe["resolved"]
            .as_array()
            .expect("every passed probe includes its actual resolved packages")
            .iter()
            .any(|package| package["name"] == name && package["version"] == version),
        "probe must resolve {name} exactly at {version}: {probe:#}",
    );
}

pub fn assert_inactive(report: &Value, name: &str) {
    let dependency = dependency(report, name);
    assert_eq!(dependency["status"], "inactive", "{dependency:#}");
    assert!(dependency["lowest_verified"].is_null(), "{dependency:#}");
    assert!(
        dependency["probes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|probe| probe["outcome"] != "passed"),
        "inactive dependencies must not be claimed compiled: {dependency:#}",
    );
}
