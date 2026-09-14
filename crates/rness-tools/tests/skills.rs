//! Skills discovery and the model-facing loader: frontmatter parsing,
//! project-shadows-user ranking, bundle resource dirs, and the tool's
//! self-describing catalog.

use std::path::Path;

use rness_tools::skills::{discover, load, SkillRoot, SkillTool};
use rness_engine::tools::Tool;

fn write_skill(dir: &Path, file: &str, name: &str, description: &str, body: &str) {
    let path = dir.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
    )
    .unwrap();
}

#[test]
fn discovers_flat_and_bundle_skills() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "commit.md", "commit", "write a commit", "Steps here.");
    write_skill(dir.path(), "deploy/SKILL.md", "deploy", "ship it", "Deploy steps.");
    std::fs::write(dir.path().join("notes.txt"), "not a skill").unwrap();

    let skills = discover(&[SkillRoot { path: dir.path().into(), rank: 0 }]);
    let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["commit", "deploy"]);
    // Bundle dir is the resource base; flat file's base is the root.
    assert_eq!(skills[1].dir, dir.path().join("deploy"));
    assert_eq!(skills[0].dir, dir.path());
}

#[test]
fn lower_rank_shadows_higher_on_name_conflict() {
    let project = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    write_skill(project.path(), "commit.md", "commit", "project version", "P");
    write_skill(user.path(), "commit.md", "commit", "user version", "U");
    write_skill(user.path(), "review.md", "review", "user only", "R");

    let skills = discover(&[
        SkillRoot { path: project.path().into(), rank: 0 },
        SkillRoot { path: user.path().into(), rank: 1 },
    ]);
    assert_eq!(skills.len(), 2);
    let commit = skills.iter().find(|s| s.name == "commit").unwrap();
    assert_eq!(commit.description, "project version");
}

#[test]
fn invalid_skills_are_skipped_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("broken.md"), "no frontmatter at all").unwrap();
    write_skill(dir.path(), "bad-name.md", "Bad_Name", "x", "y");
    write_skill(dir.path(), "good.md", "good", "works", "body");
    let skills = discover(&[SkillRoot { path: dir.path().into(), rank: 0 }]);
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "good");
}

#[test]
fn load_returns_body_without_frontmatter() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "commit.md", "commit", "d", "First line.\nSecond line.");
    let (skill, body) = load(&[SkillRoot { path: dir.path().into(), rank: 0 }], "commit").unwrap();
    assert_eq!(skill.name, "commit");
    assert_eq!(body, "First line.\nSecond line.");
}

#[tokio::test]
async fn tool_catalog_and_execute() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "deploy.md", "deploy", "ship the thing", "1. build\n2. push");
    let tool = SkillTool::new(vec![SkillRoot { path: dir.path().into(), rank: 0 }]);

    // The catalog is IN the description the model sees.
    assert!(tool.description().contains("- deploy: ship the thing"));

    let out = tool.execute(serde_json::json!({ "name": "deploy" })).await.unwrap();
    assert!(out.contains("<skill_content name=\"deploy\""));
    assert!(out.contains("1. build"));

    let err = tool.execute(serde_json::json!({ "name": "ghost" })).await.unwrap_err();
    assert!(err.contains("unknown"));
}

#[test]
fn tool_catalog_truncates_descriptions_at_utf8_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        ("a".repeat(200), "a".repeat(200)),
        ("Ó".repeat(100), "Ó".repeat(100)),
        ("a".repeat(201), format!("{}...", "a".repeat(197))),
        (format!("{}Ó extra", "a".repeat(196)), format!("{}...", "a".repeat(196))),
        (format!("{}界 extra", "a".repeat(195)), format!("{}...", "a".repeat(195))),
        (format!("{}😀 extra", "a".repeat(194)), format!("{}...", "a".repeat(194))),
    ];
    for (description, expected) in cases {
        write_skill(dir.path(), "proposal.md", "proposal", &description, "Full body.");
        let roots = vec![SkillRoot { path: dir.path().into(), rank: 0 }];
        let tool = SkillTool::new(roots.clone());
        let summary = tool.description().lines().find_map(|line| line.strip_prefix("- proposal: ")).unwrap();
        assert_eq!(summary, expected);
        assert!(summary.len() <= 200);
        let (skill, body) = load(&roots, "proposal").unwrap();
        assert_eq!(skill.description, description);
        assert_eq!(body, "Full body.");
    }
}

#[tokio::test]
async fn skill_added_after_registration_is_loadable() {
    let dir = tempfile::tempdir().unwrap();
    let tool = SkillTool::new(vec![SkillRoot { path: dir.path().into(), rank: 0 }]);
    assert!(tool.description().contains("No skills"));
    // Added AFTER the tool was built: execute re-discovers.
    write_skill(dir.path(), "late.md", "late", "arrived late", "still works");
    let out = tool.execute(serde_json::json!({ "name": "late" })).await.unwrap();
    assert!(out.contains("still works"));
}
