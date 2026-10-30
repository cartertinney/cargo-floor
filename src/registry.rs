use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use toml_edit::{DocumentMut, Item};

#[derive(Clone, Default)]
struct Source {
    replacement: Option<String>,
    registry: Option<String>,
    local: Option<PathBuf>,
    directory: bool,
}

#[derive(Default)]
pub struct CargoConfig {
    sources: BTreeMap<String, Source>,
    registries: BTreeMap<String, String>,
    pub offline: bool,
    pub targets: Vec<String>,
    pub rustc: Option<OsString>,
    pub overrides: bool,
    pub cargo_home: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub enum Registry {
    Local(PathBuf),
    Sparse(String),
    Unsupported(String),
}

impl CargoConfig {
    pub fn load(cwd: &Path) -> Result<Self> {
        let cargo_home = env::var_os("CARGO_HOME").map(PathBuf::from).or_else(|| {
            env::var_os("HOME")
                .or_else(|| env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".cargo"))
        });
        let mut config = Self {
            cargo_home,
            ..Self::default()
        };
        let mut files = Vec::new();
        if let Some(home) = &config.cargo_home {
            files.push(home.clone());
        }
        files.extend(
            cwd.ancestors()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|p| p.join(".cargo")),
        );
        for directory in files {
            let legacy = directory.join("config");
            let path = if legacy.is_file() {
                legacy
            } else {
                directory.join("config.toml")
            };
            if !path.is_file() {
                continue;
            }
            let document = fs::read_to_string(&path)
                .with_context(|| format!("read Cargo configuration {}", path.display()))?
                .parse::<DocumentMut>()
                .with_context(|| format!("parse Cargo configuration {}", path.display()))?;
            config.merge(&document, directory.parent().unwrap_or(&directory))?;
        }
        if let Ok(value) = env::var("CARGO_NET_OFFLINE") {
            config.offline = value
                .parse()
                .context("CARGO_NET_OFFLINE must be true or false")?;
        }
        if let Ok(value) = env::var("CARGO_BUILD_TARGET") {
            config.targets = vec![value];
        }
        if let Some(rustc) = env::var_os("RUSTC") {
            config.rustc = Some(rustc);
        }
        Ok(config)
    }

    fn merge(&mut self, document: &DocumentMut, base: &Path) -> Result<()> {
        if let Some(sources) = document.get("source").and_then(Item::as_table_like) {
            for (name, item) in sources.iter() {
                let source = self.sources.entry(name.to_owned()).or_default();
                if let Some(value) = string(item, "replace-with") {
                    source.replacement = Some(value.to_owned());
                }
                if let Some(value) = string(item, "registry") {
                    source.registry = Some(value.to_owned());
                }
                if let Some(value) = string(item, "local-registry") {
                    source.local = Some(base.join(value));
                }
                source.directory |= item.get("directory").is_some();
            }
        }
        if let Some(registries) = document.get("registries").and_then(Item::as_table_like) {
            for (name, item) in registries.iter() {
                if let Some(index) = string(item, "index") {
                    self.registries.insert(name.to_owned(), index.to_owned());
                }
            }
        }
        if let Some(offline) = document
            .get("net")
            .and_then(|item| item.get("offline"))
            .and_then(Item::as_bool)
        {
            self.offline = offline;
        }
        if let Some(build) = document.get("build") {
            if let Some(target) = build.get("target") {
                if let Some(target) = target.as_str() {
                    self.targets = vec![target.to_owned()];
                } else {
                    for target in target
                        .as_array()
                        .context("build.target must be a string or array")?
                    {
                        let target = target
                            .as_str()
                            .context("build.target entries must be strings")?;
                        if !self.targets.iter().any(|existing| existing == target) {
                            self.targets.push(target.to_owned());
                        }
                    }
                }
            }
            if let Some(rustc) = string(build, "rustc") {
                self.rustc = Some(if rustc.contains('/') || rustc.contains('\\') {
                    base.join(rustc).into_os_string()
                } else {
                    OsString::from(rustc)
                });
            }
        }
        self.overrides |= document.get("patch").is_some()
            || document.get("replace").is_some()
            || document.get("paths").is_some();
        Ok(())
    }

    pub fn registry(&self, index: Option<&str>) -> Result<Registry> {
        let name = match index {
            None => "crates-io".to_owned(),
            Some(index) => self
                .sources
                .iter()
                .find(|(_, source)| source.registry.as_deref() == Some(index))
                .map(|(name, _)| name.clone())
                .or_else(|| {
                    self.registries
                        .iter()
                        .find(|(_, url)| url.as_str() == index)
                        .map(|(name, _)| name.clone())
                })
                .unwrap_or_else(|| index.to_owned()),
        };
        let mut current = name;
        let mut visited = BTreeSet::new();
        loop {
            ensure!(
                visited.insert(current.clone()),
                "cycle in Cargo source replacement configuration"
            );
            if let Some(source) = self.sources.get(&current) {
                if let Some(replacement) = &source.replacement {
                    current = replacement.clone();
                    continue;
                }
                if let Some(local) = &source.local {
                    return Ok(Registry::Local(local.clone()));
                }
                if source.directory {
                    return Ok(Registry::Unsupported(
                        "directory sources do not provide a release index; use a local registry or sparse registry".into(),
                    ));
                }
                if let Some(index) = &source.registry {
                    return Ok(registry_url(index));
                }
            }
            let variable = format!(
                "CARGO_REGISTRIES_{}_INDEX",
                current.to_ascii_uppercase().replace('-', "_")
            );
            if let Ok(index) = env::var(variable) {
                return Ok(registry_url(&index));
            }
            if let Some(index) = self.registries.get(&current) {
                return Ok(registry_url(index));
            }
            return Ok(registry_url(
                index.unwrap_or("sparse+https://index.crates.io/"),
            ));
        }
    }
}

fn string<'a>(item: &'a Item, key: &str) -> Option<&'a str> {
    item.get(key).and_then(Item::as_str)
}

fn registry_url(url: &str) -> Registry {
    if let Some(url) = url.strip_prefix("sparse+") {
        if url.starts_with("https://") || url.starts_with("http://") {
            return Registry::Sparse(url.trim_end_matches('/').to_owned());
        }
    }
    if url == "https://github.com/rust-lang/crates.io-index" {
        return Registry::Sparse("https://index.crates.io".into());
    }
    Registry::Unsupported("Git registry indexes are not supported; configure a sparse registry or local-registry source".into())
}

pub struct ReleaseIndex {
    cache: BTreeMap<(String, String), Vec<Version>>,
    agent: ureq::Agent,
}

#[derive(Deserialize)]
struct Entry {
    vers: Version,
    #[serde(default)]
    yanked: bool,
}

impl ReleaseIndex {
    pub fn new() -> Self {
        Self {
            cache: BTreeMap::new(),
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(60)))
                .build()
                .into(),
        }
    }

    pub fn versions(
        &mut self,
        registry: &Registry,
        name: &str,
        offline: bool,
    ) -> Result<Vec<Version>> {
        let key = (format!("{registry:?}"), name.to_owned());
        if let Some(versions) = self.cache.get(&key) {
            return Ok(versions.clone());
        }
        let path = index_path(name);
        let text = match registry {
            Registry::Local(root) => fs::read_to_string(root.join("index").join(&path))
                .with_context(|| format!("read local registry index for {name}"))?,
            Registry::Sparse(url) => {
                ensure!(
                    !offline,
                    "cannot enumerate {name} in a sparse registry offline; configure a local-registry source"
                );
                self.agent
                    .get(format!("{url}/{path}"))
                    .header(
                        "User-Agent",
                        concat!("cargo-floor/", env!("CARGO_PKG_VERSION")),
                    )
                    .call()
                    .with_context(|| format!("fetch registry index for {name}"))?
                    .body_mut()
                    .read_to_string()
                    .with_context(|| format!("read registry index for {name}"))?
            }
            Registry::Unsupported(reason) => bail!("{reason}"),
        };
        let mut entries = BTreeMap::new();
        for (line, text) in text.lines().enumerate() {
            let entry: Entry = serde_json::from_str(text)
                .with_context(|| format!("parse {name} registry index line {}", line + 1))?;
            entries.insert(entry.vers, entry.yanked);
        }
        let versions: Vec<_> = entries
            .into_iter()
            .filter(|(version, yanked)| !yanked && version.pre.is_empty())
            .map(|(version, _)| version)
            .collect();
        self.cache.insert(key, versions.clone());
        Ok(versions)
    }
}

fn index_path(name: &str) -> String {
    let name = name.to_ascii_lowercase();
    match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    }
}
