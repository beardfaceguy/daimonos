//! Agent Skills discovery and rendering for ACP sessions.
//!
//! Skills follow the portable agentskills.io layout: immediate child
//! directories of `~/.agents/skills` and `<workspace>/.agents/skills`, each
//! containing a `SKILL.md`. Project-local skills override global skills with
//! the same name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MAX_NAME_LEN: usize = 64;
const MAX_DESCRIPTION_LEN: usize = 1024;
const MAX_CATALOG_BYTES: usize = 50 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SkillSource {
    Global,
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub directory: PathBuf,
    pub source: SkillSource,
    pub disable_model_invocation: bool,
    body: String,
}

/// Discover valid skills visible to one workspace. Invalid or unreadable files
/// are skipped and reported through tracing rather than preventing a session.
pub(crate) fn discover(workspace: &Path) -> Vec<Skill> {
    let mut by_name = BTreeMap::new();
    if let Some(home) = crate::paths::home_dir() {
        scan_root(
            &home.join(".agents/skills"),
            SkillSource::Global,
            &mut by_name,
        );
    }
    // Project entries are scanned second and intentionally override globals.
    scan_root(
        &workspace.join(".agents/skills"),
        SkillSource::Project,
        &mut by_name,
    );
    by_name.into_values().collect()
}

fn scan_root(root: &Path, source: SkillSource, skills: &mut BTreeMap<String, Skill>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path().join("SKILL.md");
        if !entry.path().is_dir() || !path.is_file() {
            continue;
        }
        match parse_skill(&path, source.clone()) {
            Ok(skill) => {
                if matches!(skill.name.as_str(), "clear" | "usage" | "help") {
                    tracing::warn!(target: "daimonos::skills", name = %skill.name, "skill name conflicts with a built-in ACP command; skipping");
                } else {
                    skills.insert(skill.name.clone(), skill);
                }
            }
            Err(error) => tracing::warn!(
                target: "daimonos::skills",
                path = %path.display(),
                error = %error,
                "skipping invalid agent skill"
            ),
        }
    }
}

fn parse_skill(path: &Path, source: SkillSource) -> Result<Skill, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let normalized = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let rest = normalized
        .strip_prefix("---\n")
        .or_else(|| normalized.strip_prefix("---\r\n"))
        .ok_or("missing YAML frontmatter")?;
    let (frontmatter, body) = rest
        .split_once("\n---\n")
        .or_else(|| rest.split_once("\r\n---\r\n"))
        .ok_or("unterminated YAML frontmatter")?;

    let mut name = None;
    let mut description = None;
    let mut disable_model_invocation = false;
    for line in frontmatter.lines() {
        let Some((key, raw_value)) = line.split_once(':') else {
            continue;
        };
        let value = unquote(raw_value.trim());
        match key.trim() {
            "name" => name = Some(value.to_string()),
            "description" => description = Some(value.to_string()),
            "disable-model-invocation" => {
                disable_model_invocation = value.eq_ignore_ascii_case("true")
            }
            _ => {}
        }
    }
    let name = name.ok_or("frontmatter is missing name")?;
    let description = description.ok_or("frontmatter is missing description")?;
    if name.is_empty()
        || name.len() > MAX_NAME_LEN
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("name must match [a-z0-9-]{1,64}".to_string());
    }
    if description.is_empty() || description.len() > MAX_DESCRIPTION_LEN {
        return Err("description must contain 1-1024 bytes".to_string());
    }
    Ok(Skill {
        name,
        description,
        directory: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        path: path.to_path_buf(),
        source,
        disable_model_invocation,
        body: body.to_string(),
    })
}

fn unquote(value: &str) -> &str {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

pub(crate) fn catalog(skills: &[Skill]) -> String {
    let mut result = String::from("\n\n<available_skills>\n");
    for skill in skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
    {
        let entry = format!(
            "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>\n",
            xml_escape(&skill.name),
            xml_escape(&skill.description),
            xml_escape(&skill.path.display().to_string()),
        );
        if result.len() + entry.len() + 20 > MAX_CATALOG_BYTES {
            tracing::warn!(target: "daimonos::skills", "agent skill catalog exceeded 50 KiB; remaining skills omitted");
            break;
        }
        result.push_str(&entry);
    }
    result.push_str("</available_skills>\nUse a skill's slash command when the user requests it. Read its SKILL.md before following referenced resources.\n");
    result
}

pub(crate) fn render(skill: &Skill) -> String {
    let source = match skill.source {
        SkillSource::Global => "global",
        SkillSource::Project => "project-local",
    };
    format!(
        "<skill_content name=\"{}\">\n<source>{source}</source>\n<directory>{}</directory>\nRelative paths in this skill resolve against <directory>.\n\n{}\n</skill_content>",
        xml_escape(&skill.name),
        xml_escape(&skill.directory.display().to_string()),
        skill.body,
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_renders_skill() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        std::fs::write(
            &path,
            "---\nname: test-skill\ndescription: Does <things>\n---\nUse scripts/run.sh & report.",
        )
        .unwrap();
        let skill = parse_skill(&path, SkillSource::Project).unwrap();
        assert_eq!(skill.name, "test-skill");
        assert!(render(&skill).contains("Use scripts/run.sh & report."));
        assert!(catalog(&[skill]).contains("Does &lt;things&gt;"));
    }

    #[test]
    fn rejects_invalid_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        std::fs::write(&path, "---\nname: Bad_Name\ndescription: no\n---\nbody").unwrap();
        assert!(parse_skill(&path, SkillSource::Global).is_err());
    }
}
