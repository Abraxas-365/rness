//! Skills: reusable instruction sets discovered from the filesystem
//! (dsh skill-filesystem + tool-skill, collapsed to our scale).
//!
//! A skill is a Markdown file with YAML-ish frontmatter (`name`,
//! `description`) in one of two shapes:
//!   <root>/<anything>.md          flat skill
//!   <root>/<name>/SKILL.md        directory bundle (resources beside it)
//!
//! Roots are ranked (lower wins on name conflicts):
//!   0  `<git-root>/.rness/skills`   project
//!   1  `<git-root>/.agents/skills`  shared agent convention
//!   2  custom dirs from Lua config  user-chosen
//!   3  `~/.rness/skills`            user
//!   4  `~/.agents/skills`           shared user agent convention
//!
//! Invalid files are skipped, not fatal — a broken skill never takes
//! discovery down.
//!
//! The catalog lives in the `skill` tool's description (specs are sent
//! to the model every step), and `execute` re-discovers, so a skill
//! added mid-session is loadable by name even before the description
//! refreshes at next compose/reload.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::tools::{Tool, ToolRegistry};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct SkillRoot {
    pub path: PathBuf,
    /// Lower wins on name conflicts.
    pub rank: u32,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The SKILL.md (or flat .md) file itself.
    pub path: PathBuf,
    /// Resource base: the bundle directory (or the root for flat files).
    pub dir: PathBuf,
    pub rank: u32,
}

/// Walk up from `cwd` to the nearest `.git` directory, falling back to
/// `cwd` itself. Replicates `rness_engine::instructions::project_root`.
pub fn project_root(cwd: &Path) -> PathBuf {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    cwd.to_path_buf()
}

/// Full skill roots for a workspace, matching dsh priority order.
/// `custom` dirs (from Lua config) are inserted at rank 2.
pub fn default_roots(workspace: &Path) -> Vec<SkillRoot> {
    default_roots_with_custom(workspace, &[])
}

/// Full skill roots including custom directories from Lua config.
pub fn default_roots_with_custom(workspace: &Path, custom: &[PathBuf]) -> Vec<SkillRoot> {
    let git_root = project_root(workspace);
    let mut roots = Vec::new();

    // Rank 0: project .rness/skills (at git root).
    roots.push(SkillRoot {
        path: git_root.join(".rness/skills"),
        rank: 0,
    });

    // Rank 1: shared agent convention (at git root).
    roots.push(SkillRoot {
        path: git_root.join(".agents/skills"),
        rank: 1,
    });

    // Rank 2: custom dirs from Lua config.
    for dir in custom {
        roots.push(SkillRoot {
            path: dir.clone(),
            rank: 2,
        });
    }

    // Rank 3-4: user dirs.
    if let Some(home) = dirs_home() {
        roots.push(SkillRoot {
            path: home.join(".rness/skills"),
            rank: 3,
        });
        roots.push(SkillRoot {
            path: home.join(".agents/skills"),
            rank: 4,
        });
    }

    roots
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Discover every valid skill under `roots`, name-deduplicated with the
/// lowest rank winning. Missing roots are simply empty.
pub fn discover(roots: &[SkillRoot]) -> Vec<Skill> {
    let mut found: Vec<Skill> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root.path) else {
            continue;
        };
        let mut names: Vec<_> = entries.flatten().collect();
        names.sort_by_key(|e| e.file_name());
        for entry in names {
            let path = entry.path();
            let candidate = if path.is_dir() {
                let f = path.join("SKILL.md");
                f.is_file().then(|| (f, path.clone()))
            } else if path.extension().is_some_and(|e| e == "md") {
                Some((path.clone(), root.path.clone()))
            } else {
                None
            };
            let Some((file, dir)) = candidate else {
                continue;
            };
            let Some((name, description)) = parse_frontmatter_summary(&file) else {
                tracing::warn!(path = %file.display(), "skill skipped: invalid frontmatter");
                continue;
            };
            if found.iter().any(|s| s.name == name) {
                continue; // earlier (lower-rank) root already claimed it
            }
            found.push(Skill {
                name,
                description,
                path: file,
                dir,
                rank: root.rank,
            });
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Frontmatter summary: `name` and `description` string fields from the
/// `---` block. Single-line values only — this is a convention, not a
/// YAML engine.
fn parse_frontmatter_summary(path: &Path) -> Option<(String, String)> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (fields, _) = split_frontmatter(&raw)?;
    let name = field(&fields, "name")?;
    let description = field(&fields, "description")?;
    valid_name(&name).then_some((name, description))
}

fn split_frontmatter(raw: &str) -> Option<(Vec<(String, String)>, String)> {
    let rest = raw.strip_prefix("---")?;
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))?;
    let end = rest.find("\n---")?;
    let yaml = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\r', '\n']).to_string();
    let mut fields = Vec::new();
    for line in yaml.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim().trim_matches('"').trim_matches('\'');
        fields.push((k.trim().to_string(), v.to_string()));
    }
    Some((fields, body))
}

fn field(fields: &[(String, String)], key: &str) -> Option<String> {
    fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty())
}

/// dsh skill-name grammar: lowercase alnum segments joined by single
/// hyphens.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('-').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

/// Load one skill's full body by exact name (re-discovers).
pub fn load(roots: &[SkillRoot], name: &str) -> Option<(Skill, String)> {
    let skill = discover(roots).into_iter().find(|s| s.name == name)?;
    let raw = std::fs::read_to_string(&skill.path).ok()?;
    let (_, body) = split_frontmatter(&raw)?;
    Some((skill, body))
}

pub fn resolve_input(
    roots: &[SkillRoot],
    mut content: Vec<rness_protocol::events::ContentPart>,
) -> Result<Vec<rness_protocol::events::ContentPart>, String> {
    use rness_protocol::events::ContentPart;
    let Some(ContentPart::Text { text }) = content.first() else {
        return Ok(content);
    };
    let (head, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let name = if head == "/skill" {
        rest.split_whitespace().next().unwrap_or("")
    } else {
        head.strip_prefix('/').unwrap_or("")
    };
    if ["agent", "unload"].contains(&name) && head != "/skill" {
        return Ok(content);
    }
    if let Some((skill, body)) = load(roots, name) {
        content.push(ContentPart::Text {
            text: format!(
                "Skill: {}\nResource directory: {}\n\n{}",
                skill.name,
                skill.dir.display(),
                body
            ),
        });
    } else if head == "/skill" {
        return Err(format!("Unknown skill: {name}"));
    }
    Ok(content)
}

/// The model-facing loader. The available catalog is part of this
/// tool's DESCRIPTION — computed at registration, refreshed whenever
/// the tool is re-registered (compose, hot reload).
pub struct SkillTool {
    roots: Vec<SkillRoot>,
    /// Custom dirs from Lua config, passed through to `for_workspace`.
    custom: Vec<PathBuf>,
    description: String,
}

impl SkillTool {
    pub fn new(roots: Vec<SkillRoot>) -> Self {
        Self::with_custom(roots, Vec::new())
    }

    pub fn with_custom(roots: Vec<SkillRoot>, custom: Vec<PathBuf>) -> Self {
        let skills = discover(&roots);
        let mut description = String::from(
            "Load the full instructions for an available skill. Call this with \
             the exact skill name before acting on a task that names or clearly \
             matches that skill; follow the loaded instructions.",
        );
        if skills.is_empty() {
            description.push_str(" No skills are currently available.");
        } else {
            description.push_str("\nAvailable skills:\n");
            for s in &skills {
                let d = s.description.replace('\n', " ");
                let d = if d.len() > 200 {
                    let mut end = 197;
                    // Keep the byte budget without splitting a UTF-8 character.
                    while !d.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}...", &d[..end])
                } else {
                    d
                };
                description.push_str(&format!("- {}: {}\n", s.name, d));
            }
        }
        Self { roots, custom, description }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn for_workspace(
        &self,
        _session: &String,
        workspace: &std::path::Path,
    ) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self::with_custom(
            default_roots_with_custom(workspace, &self.custom),
            self.custom.clone(),
        )))
    }
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Exact skill name from the available skills list",
                },
            },
            "required": ["name"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, String> {
        self.load_presented(args).map(|(output, _)| output)
    }

    async fn execute_presented(
        &self,
        _session: &String,
        _call: &String,
        args: Value,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<
        (
            Vec<rness_protocol::events::ToolResultContentPart>,
            Option<rness_protocol::events::TaskSnapshot>,
            bool,
            Option<Value>,
        ),
        String,
    > {
        let (output, metadata) = self.load_presented(args)?;
        Ok((
            vec![rness_protocol::events::ToolResultContentPart::Text { text: output }],
            None,
            false,
            Some(metadata),
        ))
    }
}

impl SkillTool {
    fn load_presented(&self, args: Value) -> Result<(String, Value), String> {
        let name = crate::required_str(&args, "name")?;
        let (skill, body) = load(&self.roots, name)
            .ok_or_else(|| format!("skill '{name}' is unknown or no longer available"))?;
        let metadata = json!({"version":1,"kind":"skill","name":skill.name,"path":skill.path,"resource_dir":skill.dir,"body_bytes":body.len()});
        Ok((
            format!(
                "<skill_content name=\"{}\" resource_dir=\"{}\">\n{}\n</skill_content>",
                skill.name,
                skill.dir.display(),
                body.trim_end(),
            ),
            metadata,
        ))
    }
}

/// Register the skill loader (composition seam — the host decides the
/// roots, nothing is implicit).
pub fn register_skills(registry: &ToolRegistry, roots: Vec<SkillRoot>) {
    registry.register(Arc::new(SkillTool::new(roots)));
}

/// Register the skill loader with custom directories from Lua config.
pub fn register_skills_with_custom(
    registry: &ToolRegistry,
    roots: Vec<SkillRoot>,
    custom: Vec<PathBuf>,
) {
    registry.register(Arc::new(SkillTool::with_custom(roots, custom)));
}
