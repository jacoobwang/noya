//! Discovery and validation of prompt-level Skill packages.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const MAX_SKILL_BYTES: usize = 64 * 1024;
const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSource {
    Project,
    User,
}

impl std::fmt::Display for SkillSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Project => "project",
            Self::User => "user",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    pub path: PathBuf,
    pub digest: String,
    pub disable_model_invocation: bool,
}

#[derive(Debug, Clone)]
struct Skill {
    info: SkillInfo,
    body: String,
}

#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: BTreeMap<String, Skill>,
    warnings: Vec<String>,
}

impl SkillRegistry {
    pub fn discover(workspace: &Path) -> Result<Self> {
        let mut registry = Self::default();
        registry.scan_root(&workspace.join(".agents/skills"), SkillSource::Project)?;
        let home = dirs::home_dir().context("cannot determine the user home directory")?;
        registry.scan_root(&home.join(".noya/skills"), SkillSource::User)?;
        Ok(registry)
    }

    pub fn list(&self) -> Vec<SkillInfo> {
        self.skills.values().map(|skill| skill.info.clone()).collect()
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn get(&self, name: &str) -> Option<&SkillInfo> {
        self.skills.get(name).map(|skill| &skill.info)
    }

    pub fn body(&self, name: &str) -> Option<&str> {
        self.skills.get(name).map(|skill| skill.body.as_str())
    }

    fn scan_root(&mut self, root: &Path, source: SkillSource) -> Result<()> {
        if !root.is_dir() {
            return Ok(());
        }
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalize skills root {}", root.display()))?;
        for entry in fs::read_dir(&root).with_context(|| format!("read skills root {}", root.display()))? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = path.file_name().and_then(|value| value.to_str()).unwrap_or_default();
            let skill_path = path.join("SKILL.md");
            match load_skill(&root, &skill_path, name, source) {
                Ok(skill) => {
                    // Project roots are scanned first, so a user Skill never replaces one.
                    self.skills.entry(skill.info.name.clone()).or_insert(skill);
                }
                Err(error) => self.warnings.push(format!(
                    "skip invalid {} Skill {}: {error}",
                    source,
                    path.display()
                )),
            }
        }
        Ok(())
    }
}

fn load_skill(root: &Path, path: &Path, directory_name: &str, source: SkillSource) -> Result<Skill> {
    ensure!(valid_name(directory_name), "invalid Skill ID '{directory_name}'");
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize skills root {}", root.display()))?;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("read {}", path.display()))?;
    ensure!(canonical.starts_with(root), "SKILL.md resolves outside its skills root");
    ensure!(canonical.is_file(), "SKILL.md is not a file");
    let metadata = fs::metadata(&canonical)?;
    ensure!(metadata.len() as usize <= MAX_SKILL_BYTES, "SKILL.md exceeds {MAX_SKILL_BYTES} bytes");
    let source_text = fs::read_to_string(&canonical)
        .with_context(|| format!("read Skill {}", canonical.display()))?;
    let (frontmatter, body) = split_frontmatter(&source_text)?;
    let fields = parse_frontmatter(frontmatter)?;
    let name = fields.get("name").context("frontmatter is missing name")?.clone();
    ensure!(name == directory_name, "frontmatter name must match directory name");
    ensure!(valid_name(&name), "invalid Skill ID '{name}'");
    let description = fields
        .get("description")
        .context("frontmatter is missing description")?
        .clone();
    ensure!(!description.is_empty(), "Skill description cannot be empty");
    ensure!(description.chars().count() <= MAX_DESCRIPTION_LENGTH, "Skill description is too long");
    let disable_model_invocation = fields
        .get("disable-model-invocation")
        .map(|value| value.parse::<bool>().context("disable-model-invocation must be true or false"))
        .transpose()?
        .unwrap_or(false);
    Ok(Skill {
        info: SkillInfo {
            name,
            description,
            source,
            path: canonical,
            digest: digest(&source_text),
            disable_model_invocation,
        },
        body: body.trim().to_string(),
    })
}

fn split_frontmatter(source: &str) -> Result<(&str, &str)> {
    let Some(rest) = source.strip_prefix("---\n") else {
        bail!("SKILL.md must start with YAML frontmatter")
    };
    let Some(end) = rest.find("\n---\n") else {
        bail!("SKILL.md frontmatter is not closed")
    };
    Ok((&rest[..end], &rest[end + 5..]))
}

fn parse_frontmatter(source: &str) -> Result<BTreeMap<String, String>> {
    let mut fields = BTreeMap::new();
    for line in source.lines() {
        let Some((key, value)) = line.split_once(':') else {
            bail!("invalid frontmatter line '{line}'")
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"').trim_matches('\'');
        ensure!(!key.is_empty(), "frontmatter key cannot be empty");
        ensure!(fields.insert(key.to_string(), value.to_string()).is_none(), "duplicate frontmatter key '{key}'");
    }
    Ok(fields)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_LENGTH
        && name.chars().all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-')
}

fn digest(value: &str) -> String {
    // A deterministic, dependency-free content fingerprint for session audit records.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a-{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn discovers_valid_skill_and_parses_metadata() {
        let root = tempdir().unwrap();
        let skill_dir = root.path().join("demo");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: A demo skill\ndisable-model-invocation: true\n---\n\nDo the thing.\n",
        )
        .unwrap();
        let skill = load_skill(root.path(), &skill_dir.join("SKILL.md"), "demo", SkillSource::Project).unwrap();
        assert_eq!(skill.info.name, "demo");
        assert!(skill.info.disable_model_invocation);
        assert_eq!(skill.body, "Do the thing.");
    }

    #[test]
    fn rejects_name_mismatch() {
        let root = tempdir().unwrap();
        let skill_dir = root.path().join("demo");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "---\nname: other\ndescription: bad\n---\nbody").unwrap();
        assert!(load_skill(root.path(), &skill_dir.join("SKILL.md"), "demo", SkillSource::Project).is_err());
    }
}
