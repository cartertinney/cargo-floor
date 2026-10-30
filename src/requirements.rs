use anyhow::{Result, bail, ensure};
use semver::{Comparator, Op, Version, VersionReq};

#[derive(Clone)]
pub struct SearchWindow {
    minimum: Version,
    upper_bounds: Vec<Comparator>,
    original: String,
    caret: bool,
    anchor: Version,
}

impl SearchWindow {
    pub fn new(original: &str, resolved: &Version) -> Result<Self> {
        let requirement = VersionReq::parse(original)?;
        ensure!(
            requirement.comparators.iter().all(|c| c.pre.is_empty()),
            "prerelease requirements are not supported"
        );
        let anchor = requirement
            .comparators
            .iter()
            .filter(|c| {
                matches!(
                    c.op,
                    Op::Caret | Op::Tilde | Op::Wildcard | Op::Greater | Op::GreaterEq | Op::Exact
                )
            })
            .map(version)
            .max()
            .ok_or_else(|| {
                anyhow::anyhow!("a lower bound is needed to identify a compatibility line")
            })?;
        let (minimum, end) = compatibility_line(&anchor)?;
        ensure!(
            resolved >= &minimum && resolved < &end && !requirement.matches(&end),
            "requirement spans multiple SemVer compatibility lines; use a bounded requirement"
        );
        let mut upper_bounds = vec![Comparator::parse(&format!("<{end}"))?];
        for comparator in &requirement.comparators {
            let upper = match comparator.op {
                Op::Less | Op::LessEq => Some(comparator.clone()),
                Op::Caret => Some(Comparator::parse(&format!("<{}", caret_end(comparator)?))?),
                Op::Tilde | Op::Wildcard => {
                    let end = match comparator.minor {
                        Some(minor) => Version::new(comparator.major, increment(minor)?, 0),
                        None => Version::new(increment(comparator.major)?, 0, 0),
                    };
                    Some(Comparator::parse(&format!("<{end}"))?)
                }
                Op::Exact => bail!("exact version pins are not lower-bound requirements"),
                Op::Greater | Op::GreaterEq => None,
                _ => bail!("unsupported version comparator"),
            };
            if let Some(upper) = upper {
                if !upper_bounds.contains(&upper) {
                    upper_bounds.push(upper);
                }
            }
        }
        Ok(Self {
            minimum,
            upper_bounds,
            original: original.to_owned(),
            caret: requirement.comparators.len() == 1 && requirement.comparators[0].op == Op::Caret,
            anchor,
        })
    }

    pub fn contains(&self, version: &Version) -> bool {
        version.pre.is_empty()
            && version >= &self.minimum
            && self.upper_bounds.iter().all(|bound| bound.matches(version))
    }

    pub fn expanded(&self) -> String {
        self.bounded(&self.minimum)
    }

    pub fn proposed(&self, version: &Version) -> String {
        if self.caret {
            if version == &self.anchor {
                return self.original.clone();
            }
            return version.to_string();
        }
        self.bounded(version)
    }

    fn bounded(&self, version: &Version) -> String {
        std::iter::once(format!(">={version}"))
            .chain(self.upper_bounds.iter().map(ToString::to_string))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn version(comparator: &Comparator) -> Version {
    Version::new(
        comparator.major,
        comparator.minor.unwrap_or(0),
        comparator.patch.unwrap_or(0),
    )
}

fn increment(component: u64) -> Result<u64> {
    component
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("version component is too large"))
}

fn compatibility_line(anchor: &Version) -> Result<(Version, Version)> {
    if anchor.major > 0 {
        Ok((
            Version::new(anchor.major, 0, 0),
            Version::new(increment(anchor.major)?, 0, 0),
        ))
    } else if anchor.minor > 0 {
        Ok((
            Version::new(0, anchor.minor, 0),
            Version::new(0, increment(anchor.minor)?, 0),
        ))
    } else {
        Ok((
            Version::new(0, 0, anchor.patch),
            Version::new(0, 0, increment(anchor.patch)?),
        ))
    }
}

fn caret_end(comparator: &Comparator) -> Result<Version> {
    if comparator.major > 0 || comparator.minor.is_none() {
        Ok(Version::new(increment(comparator.major)?, 0, 0))
    } else if comparator.minor != Some(0) || comparator.patch.is_none() {
        Ok(Version::new(
            0,
            increment(comparator.minor.unwrap_or(0))?,
            0,
        ))
    } else {
        Ok(Version::new(
            0,
            0,
            increment(comparator.patch.unwrap_or(0))?,
        ))
    }
}
