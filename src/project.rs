use std::path::{Component, Path, PathBuf};

use serde_json::Value;

const WORKING_DIRECTORY_PREFIXES: [&str; 3] = [
    "Primary working directory:",
    "Working directory:",
    "Current working directory:",
];

/// The project a working directory belongs to, and the worktree of it when the
/// directory is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectName {
    pub project: String,
    pub worktree: Option<String>,
}

impl ProjectName {
    fn without_worktree(project: String) -> Self {
        Self {
            project,
            worktree: None,
        }
    }
}

pub fn name_from_system(system: Option<&Value>) -> Option<ProjectName> {
    system.and_then(name_from_value)
}

pub fn name_from_request<'a>(
    system: Option<&Value>,
    message_contents: impl IntoIterator<Item = &'a Value>,
) -> Option<ProjectName> {
    name_from_system(system).or_else(|| message_contents.into_iter().find_map(name_from_value))
}

fn name_from_value(value: &Value) -> Option<ProjectName> {
    match value {
        Value::String(text) => name_from_text(text),
        Value::Array(values) => values.iter().find_map(name_from_value),
        Value::Object(object) => object
            .get("text")
            .and_then(Value::as_str)
            .and_then(name_from_text)
            .or_else(|| object.get("content").and_then(name_from_value)),
        _ => None,
    }
}

fn name_from_text(text: &str) -> Option<ProjectName> {
    text.lines()
        .find_map(working_directory_from_line)
        .and_then(name_from_working_directory)
}

fn working_directory_from_line(line: &str) -> Option<&str> {
    let line = line.trim().strip_prefix("- ").unwrap_or(line.trim());
    WORKING_DIRECTORY_PREFIXES.iter().find_map(|prefix| {
        line.strip_prefix(prefix)
            .map(str::trim)
            .filter(|path| !path.is_empty())
    })
}

/// The repository on this machine the directory sits in, else the Claude Code
/// worktree its path names, else the directory's own name. A session running
/// on another machine has nothing on disk here, so only its path says what it
/// is.
fn name_from_working_directory(path: &str) -> Option<ProjectName> {
    let working_directory = Path::new(path);
    let repository_root = working_directory
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists());

    match repository_root {
        Some(root) => repository_name(root),
        None => claude_worktree_name(working_directory)
            .or_else(|| path_name(working_directory).map(ProjectName::without_worktree)),
    }
}

fn repository_name(root: &Path) -> Option<ProjectName> {
    let git_marker = root.join(".git");
    if git_marker.is_dir() {
        return path_name(root).map(ProjectName::without_worktree);
    }

    main_repository_name(root, &git_marker)
        .or_else(|| path_name(root).map(ProjectName::without_worktree))
}

/// The main repository of a directory whose `.git` is a file. The directory is
/// a worktree of it only when that file points into the repository's
/// `worktrees`, not, say, into its `modules` as a submodule's does.
fn main_repository_name(root: &Path, git_marker: &Path) -> Option<ProjectName> {
    let contents = std::fs::read_to_string(git_marker).ok()?;
    let git_dir = contents.trim().strip_prefix("gitdir:")?.trim();
    let git_dir = if Path::new(git_dir).is_absolute() {
        PathBuf::from(git_dir)
    } else {
        root.join(git_dir)
    };
    let main_git_dir = git_dir
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == ".git"))?;
    let project = main_git_dir.parent().and_then(path_name)?;
    let linked = git_dir.parent() == Some(main_git_dir.join("worktrees").as_path());
    let worktree = path_name(root).filter(|name| linked && *name != project);
    Some(ProjectName { project, worktree })
}

/// A worktree Claude Code made, as its path names it:
/// `<repository>/.claude/worktrees/<name>`, or a directory inside one.
fn claude_worktree_name(path: &Path) -> Option<ProjectName> {
    let components: Vec<&str> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect();
    components.windows(4).find_map(|window| {
        let [project, dot_claude, worktrees, worktree] = window else {
            return None;
        };
        (*dot_claude == ".claude" && *worktrees == "worktrees").then(|| ProjectName {
            project: (*project).to_string(),
            worktree: Some((*worktree).to_string()),
        })
    })
}

fn path_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn reads_primary_working_directory_from_claude_code_system_blocks() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        let system = json!([
            {
                "type": "text",
                "text": "x-anthropic-billing-header: cc_version=2.1.177.45c"
            },
            {
                "type": "text",
                "text": "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
                "cache_control": {"type": "ephemeral"}
            },
            {
                "type": "text",
                "text": format!(
                    "\nYou are an interactive agent.\n\n# Environment\nYou have been invoked in the following environment: \n - Primary working directory: {}\n - Is a git repository: true",
                    root.path().display()
                ),
                "cache_control": {"type": "ephemeral"}
            }
        ]);

        assert_eq!(
            project_of(name_from_system(Some(&system))).as_deref(),
            root.path().file_name().and_then(|name| name.to_str())
        );
    }

    fn project_of(name: Option<ProjectName>) -> Option<String> {
        name.map(|name| name.project)
    }

    #[test]
    fn reads_legacy_working_directory_from_string_system_prompt() {
        let system = json!("<env>\nWorking directory: /home/user/example\n</env>");

        assert_eq!(
            project_of(name_from_system(Some(&system))).as_deref(),
            Some("example")
        );
    }

    #[test]
    fn reads_working_directory_from_message_system_reminder() {
        let content = json!([
            {"type": "text", "text": "hello"},
            {"type": "text", "text": "<system-reminder>\n# Environment\n - Primary working directory: /home/user/example\n</system-reminder>"}
        ]);

        assert_eq!(
            project_of(name_from_request(None, [&content])).as_deref(),
            Some("example")
        );
    }

    #[test]
    fn resolves_linked_worktree_to_main_repository_name() {
        let temp = tempdir().unwrap();
        let main = temp.path().join("project");
        let worktree = temp.path().join("worktrees").join("feature");
        let git_dir = main.join(".git").join("worktrees").join("feature");
        fs::create_dir_all(&git_dir).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();

        assert_eq!(
            name_from_working_directory(worktree.to_str().unwrap()),
            Some(ProjectName {
                project: "project".to_string(),
                worktree: Some("feature".to_string()),
            })
        );
        // A directory inside the worktree belongs to the same one.
        let inside = worktree.join("src");
        fs::create_dir_all(&inside).unwrap();
        assert_eq!(
            name_from_working_directory(inside.to_str().unwrap())
                .and_then(|name| name.worktree)
                .as_deref(),
            Some("feature")
        );
    }

    #[test]
    fn a_submodule_is_not_a_worktree() {
        let temp = tempdir().unwrap();
        let main = temp.path().join("project");
        let module = main.join("vendor").join("lib");
        let git_dir = main.join(".git").join("modules").join("lib");
        fs::create_dir_all(&git_dir).unwrap();
        fs::create_dir_all(&module).unwrap();
        fs::write(
            module.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();

        assert_eq!(
            name_from_working_directory(module.to_str().unwrap()),
            Some(ProjectName::without_worktree("project".to_string()))
        );
    }

    #[test]
    fn a_claude_code_worktree_path_names_its_repository_and_worktree() {
        let system = json!("<env>\nWorking directory: /home/u/repo/.claude/worktrees/wt\n</env>");

        assert_eq!(
            name_from_system(Some(&system)),
            Some(ProjectName {
                project: "repo".to_string(),
                worktree: Some("wt".to_string()),
            })
        );
        assert_eq!(
            name_from_working_directory("/home/u/repo/.claude/worktrees/wt/crates/core"),
            Some(ProjectName {
                project: "repo".to_string(),
                worktree: Some("wt".to_string()),
            })
        );
        assert_eq!(
            name_from_working_directory("/home/u/repo/.claude"),
            Some(ProjectName::without_worktree(".claude".to_string()))
        );
    }

    #[test]
    fn returns_none_without_working_directory_metadata() {
        assert_eq!(name_from_system(Some(&json!("instructions"))), None);
        assert_eq!(name_from_system(None), None);
    }
}
