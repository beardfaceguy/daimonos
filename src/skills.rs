//! Portable Agent Skills discovery and activation.
//!
//! A skill is one directory containing an exact `SKILL.md` filename. The file
//! is Markdown with YAML frontmatter. Only `name` and `description` are required;
//! unknown metadata is retained so skills authored for another harness remain
//! usable. Skill bodies are read only when activated.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SKILL_FILE: &str = "SKILL.md";
pub const DEFAULT_SKILL_DIR: &str = ".agents/skills";
pub const SKILL_DIR_ENV: &str = "DAIMONOS_SKILL_DIR";
pub const MAX_SKILL_BYTES: u64 = 100 * 1024;
pub const RECOMMENDED_DESCRIPTION_BYTES: usize = 1024;
pub const CATALOG_MAX_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    Global,
    Workspace,
}

impl SkillSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Workspace => "workspace",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub disable_model_invocation: bool,
    /// Harness-specific extension fields, deliberately retained.
    #[allow(dead_code)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub metadata: SkillMetadata,
    pub source: SkillSource,
    pub directory: PathBuf,
    pub file: PathBuf,
}

#[derive(Debug, Default)]
pub struct Discovery {
    /// Effective skills after deterministic workspace-over-global precedence.
    pub skills: Vec<Skill>,
    /// Every valid source, for future source-qualified manual invocation.
    pub all_sources: Vec<Skill>,
    pub warnings: Vec<String>,
}

/// Resolve the global root. A missing or blank override safely falls back to
/// `~/.agents/skills`; a relative override is rejected because its meaning
/// would vary with the process cwd.
pub fn global_root_from(raw: Option<&str>, home: Option<&Path>) -> Result<PathBuf, String> {
    let raw = raw.map(str::trim).filter(|v| !v.is_empty());
    match raw {
        Some(value) => {
            let path = match value.strip_prefix("~/") {
                Some(rest) => home
                    .map(|h| h.join(rest))
                    .ok_or_else(|| format!("{SKILL_DIR_ENV} uses '~' but HOME is unavailable"))?,
                None => PathBuf::from(value),
            };
            if !path.is_absolute() {
                return Err(format!(
                    "{SKILL_DIR_ENV} must be an absolute path (or start with ~/): {value}"
                ));
            }
            Ok(path)
        }
        None => home.map(|h| h.join(DEFAULT_SKILL_DIR)).ok_or_else(|| {
            format!(
                "cannot resolve default skill directory ~/{DEFAULT_SKILL_DIR}: HOME is unavailable"
            )
        }),
    }
}

pub fn global_root() -> Result<PathBuf, String> {
    let raw = std::env::var(SKILL_DIR_ENV).ok();
    global_root_from(raw.as_deref(), crate::paths::home_dir().as_deref())
}

pub fn discover(workspace: &Path) -> Discovery {
    match global_root() {
        Ok(root) => discover_in(workspace, &root),
        Err(error) => Discovery {
            warnings: vec![error],
            ..Discovery::default()
        },
    }
}

pub fn discover_in(workspace: &Path, global_root: &Path) -> Discovery {
    let workspace_root = workspace.join(DEFAULT_SKILL_DIR);
    let mut out = Discovery::default();
    scan_root(global_root, SkillSource::Global, &mut out);
    scan_root(&workspace_root, SkillSource::Workspace, &mut out);
    out.all_sources.sort_by(|a, b| {
        a.metadata
            .name
            .cmp(&b.metadata.name)
            .then_with(|| a.source.label().cmp(b.source.label()))
            .then_with(|| a.file.cmp(&b.file))
    });

    let mut effective = BTreeMap::<String, Skill>::new();
    for skill in &out.all_sources {
        // Global is scanned first, so workspace deterministically replaces it.
        effective.insert(skill.metadata.name.clone(), skill.clone());
    }
    out.skills = effective.into_values().collect();
    out
}

fn scan_root(root: &Path, source: SkillSource, out: &mut Discovery) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            out.warnings.push(format!(
                "skill directory {} is unreadable: {error}",
                root.display()
            ));
            return;
        }
    };
    let mut dirs = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    dirs.sort();
    for directory in dirs {
        let file = directory.join(SKILL_FILE);
        if !file.is_file() {
            continue;
        }
        match parse_metadata(&file, source, directory) {
            Ok((skill, warning)) => {
                if let Some(warning) = warning {
                    out.warnings.push(warning);
                }
                out.all_sources.push(skill);
            }
            Err(error) => out.warnings.push(error),
        }
    }
}

fn parse_metadata(
    file: &Path,
    source: SkillSource,
    directory: PathBuf,
) -> Result<(Skill, Option<String>), String> {
    let len = std::fs::metadata(file)
        .map_err(|e| format!("skill {} metadata unreadable: {e}", file.display()))?
        .len();
    if len > MAX_SKILL_BYTES {
        return Err(format!(
            "skill {} is {len} bytes; maximum is {MAX_SKILL_BYTES}",
            file.display()
        ));
    }
    let content = std::fs::read_to_string(file)
        .map_err(|e| format!("skill {} unreadable: {e}", file.display()))?;
    let (frontmatter, _) =
        split_frontmatter(&content).map_err(|e| format!("skill {}: {e}", file.display()))?;
    let raw: serde_yaml::Value = serde_yaml::from_str(frontmatter).map_err(|e| {
        format!(
            "skill {} has malformed YAML frontmatter: {e}",
            file.display()
        )
    })?;
    let mut map = raw.as_mapping().cloned().ok_or_else(|| {
        format!(
            "skill {} frontmatter must be a YAML mapping",
            file.display()
        )
    })?;
    let name = take_string(&mut map, "name", file)?;
    validate_name(&name).map_err(|e| format!("skill {}: {e}", file.display()))?;
    let description = take_string(&mut map, "description", file)?;
    if description.trim().is_empty() {
        return Err(format!(
            "skill {}: description must not be blank",
            file.display()
        ));
    }
    let disable_model_invocation =
        take_bool(&mut map, "disable-model-invocation", file)?.unwrap_or(false);
    let mut extensions = BTreeMap::new();
    for (key, value) in map {
        if let Some(key) = key.as_str() {
            let value = serde_json::to_value(value).unwrap_or(Value::Null);
            extensions.insert(key.to_string(), value);
        }
    }
    let warning = (description.len() > RECOMMENDED_DESCRIPTION_BYTES).then(|| {
        format!(
            "skill {} description is {} bytes; recommended maximum is {}",
            file.display(),
            description.len(),
            RECOMMENDED_DESCRIPTION_BYTES
        )
    });
    Ok((
        Skill {
            metadata: SkillMetadata {
                name,
                description,
                disable_model_invocation,
                extensions,
            },
            source,
            directory,
            file: file.to_path_buf(),
        },
        warning,
    ))
}

fn split_frontmatter(content: &str) -> Result<(&str, &str), &'static str> {
    let rest = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .ok_or("missing opening YAML frontmatter delimiter '---'")?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            return Ok((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    Err("missing closing YAML frontmatter delimiter '---'")
}

fn yaml_key(key: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(key.to_string())
}

fn take_string(map: &mut serde_yaml::Mapping, key: &str, file: &Path) -> Result<String, String> {
    match map.remove(yaml_key(key)) {
        Some(serde_yaml::Value::String(value)) => Ok(value),
        Some(_) => Err(format!(
            "skill {}: '{key}' must be a string",
            file.display()
        )),
        None => Err(format!(
            "skill {}: missing required frontmatter field '{key}'",
            file.display()
        )),
    }
}

fn take_bool(
    map: &mut serde_yaml::Mapping,
    key: &str,
    file: &Path,
) -> Result<Option<bool>, String> {
    match map.remove(yaml_key(key)) {
        Some(serde_yaml::Value::Bool(value)) => Ok(Some(value)),
        Some(_) => Err(format!(
            "skill {}: '{key}' must be a boolean",
            file.display()
        )),
        None => Ok(None),
    }
}

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    if name.len() > 64 {
        return Err("name must be at most 64 bytes".into());
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("name must not start or end with '-'".into());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err("name may contain only ASCII lowercase letters, digits, and hyphens".into());
    }
    Ok(())
}

pub fn catalog(discovery: &Discovery, max_bytes: usize) -> Option<String> {
    let eligible = discovery
        .skills
        .iter()
        .filter(|s| !s.metadata.disable_model_invocation)
        .collect::<Vec<_>>();
    if eligible.is_empty() {
        return None;
    }
    let header = "## Available agent skills\n\nSkill bodies are not loaded. Call the `skill` tool with a name when its instructions are relevant.\n\n";
    let mut out = header.to_string();
    let mut omitted = 0usize;
    for skill in eligible {
        let line = format!(
            "- `{}`: {}\n",
            skill.metadata.name,
            skill.metadata.description.trim()
        );
        if out.len() + line.len() > max_bytes {
            omitted += 1;
        } else {
            out.push_str(&line);
        }
    }
    if omitted > 0 {
        let notice = format!(
            "\n[{omitted} skill(s) omitted because the catalog reached its byte budget.]\n"
        );
        if out.len() + notice.len() <= max_bytes {
            out.push_str(&notice);
        }
    }
    Some(out)
}

pub fn expand_manual_invocation(workspace: &Path, text: &str) -> Result<String, String> {
    let Some(command) = text.strip_prefix('/') else {
        return Ok(text.to_string());
    };
    let (name, remainder) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    if name.is_empty() {
        return Ok(text.to_string());
    }
    // Reserved harness commands remain owned by their frontends. Unknown valid
    // skill names are left untouched so other slash-command integrations can run.
    if matches!(name, "exit" | "quit" | "clear" | "help" | "usage") {
        return Ok(text.to_string());
    }
    validate_name(name)?;
    let discovery = discover(workspace);
    if !discovery
        .skills
        .iter()
        .any(|skill| skill.metadata.name == name)
    {
        return Ok(text.to_string());
    }
    let envelope = activation_envelope(workspace, name)?;
    if remainder.trim().is_empty() {
        Ok(envelope)
    } else {
        Ok(format!("{envelope}\n\n{}", remainder.trim_start()))
    }
}

pub fn activation_envelope(workspace: &Path, name: &str) -> Result<String, String> {
    validate_name(name)?;
    let discovery = discover(workspace);
    let skill = discovery
        .skills
        .iter()
        .find(|s| s.metadata.name == name)
        .ok_or_else(|| {
            let names = discovery
                .skills
                .iter()
                .map(|s| s.metadata.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if names.is_empty() {
                format!("skill '{name}' not found; no valid skills were discovered")
            } else {
                format!("skill '{name}' not found; available skills: {names}")
            }
        })?;
    let content = std::fs::read_to_string(&skill.file)
        .map_err(|e| format!("skill {} unreadable: {e}", skill.file.display()))?;
    if content.len() as u64 > MAX_SKILL_BYTES {
        return Err(format!(
            "skill '{}' exceeds the {MAX_SKILL_BYTES}-byte maximum",
            skill.metadata.name
        ));
    }
    let (_, body) =
        split_frontmatter(&content).map_err(|e| format!("skill {}: {e}", skill.file.display()))?;
    Ok(format!(
        "<agent_skill name=\"{}\" source=\"{}\" directory=\"{}\">\n{}\n</agent_skill>",
        skill.metadata.name,
        skill.source.label(),
        skill.directory.display(),
        body.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, dir: &str, frontmatter: &str, body: &str) {
        let path = root.join(dir);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join(SKILL_FILE),
            format!("---\n{frontmatter}\n---\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn global_root_defaults_and_override_are_safe() {
        let home = Path::new("/home/tester");
        assert_eq!(
            global_root_from(None, Some(home)).unwrap(),
            home.join(DEFAULT_SKILL_DIR)
        );
        assert_eq!(
            global_root_from(Some("  "), Some(home)).unwrap(),
            home.join(DEFAULT_SKILL_DIR)
        );
        assert_eq!(
            global_root_from(Some("~/portable skills"), Some(home)).unwrap(),
            home.join("portable skills")
        );
        assert!(global_root_from(Some("relative/skills"), Some(home)).is_err());
    }

    #[test]
    fn discovers_one_level_with_workspace_precedence_and_extensions() {
        let temp = tempfile::tempdir().unwrap();
        let global = temp.path().join("global");
        let workspace = temp.path().join("workspace");
        write_skill(
            &global,
            "foo",
            "name: foo\ndescription: global\nx-other: kept",
            "global body",
        );
        write_skill(
            &workspace.join(DEFAULT_SKILL_DIR),
            "foo",
            "name: foo\ndescription: local",
            "local body",
        );
        write_skill(
            &global.join("group"),
            "nested",
            "name: nested\ndescription: nope",
            "nested",
        );
        let found = discover_in(&workspace, &global);
        assert_eq!(found.all_sources.len(), 2);
        assert_eq!(found.skills.len(), 1);
        assert_eq!(found.skills[0].metadata.description, "local");
        assert!(found.all_sources[0]
            .metadata
            .extensions
            .contains_key("x-other"));
    }

    #[test]
    fn catalog_excludes_manual_only_and_is_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let global = temp.path().join("global");
        write_skill(
            &global,
            "visible",
            "name: visible\ndescription: shown",
            "body",
        );
        write_skill(
            &global,
            "manual",
            "name: manual\ndescription: hidden\ndisable-model-invocation: true",
            "body",
        );
        let found = discover_in(temp.path(), &global);
        let text = catalog(&found, 4096).unwrap();
        assert!(text.contains("visible"));
        assert!(!text.contains("manual"));
        assert!(catalog(&found, 140).unwrap().len() <= 140);
    }

    #[test]
    fn manual_invocation_preserves_arguments_and_allows_manual_only() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path();
        write_skill(
            &workspace.join(DEFAULT_SKILL_DIR),
            "manual",
            "name: manual\ndescription: explicit\ndisable-model-invocation: true",
            "do the thing",
        );
        let expanded = expand_manual_invocation(workspace, "/manual argument text").unwrap();
        assert!(expanded.contains("do the thing"));
        assert!(expanded.ends_with("argument text"));
        assert_eq!(
            expand_manual_invocation(workspace, "/unknown value").unwrap(),
            "/unknown value"
        );
        assert_eq!(
            expand_manual_invocation(workspace, "/help").unwrap(),
            "/help"
        );
    }

    #[test]
    fn invalid_and_oversized_skills_warn_without_breaking_valid_ones() {
        let temp = tempfile::tempdir().unwrap();
        let global = temp.path().join("global");
        write_skill(&global, "good", "name: good\ndescription: valid", "body");
        write_skill(
            &global,
            "bad",
            "name: Bad_Name\ndescription: invalid",
            "body",
        );
        let huge = global.join("huge");
        std::fs::create_dir_all(&huge).unwrap();
        std::fs::write(
            huge.join(SKILL_FILE),
            vec![b'x'; MAX_SKILL_BYTES as usize + 1],
        )
        .unwrap();
        let found = discover_in(temp.path(), &global);
        assert_eq!(found.skills.len(), 1);
        assert_eq!(found.warnings.len(), 2);
    }
}
