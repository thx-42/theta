//! Skills: markdown rule files from ~/.theta/skills (global) and <project>/.theta/skills (local).
//! A skill is `<name>.md` or `<name>/SKILL.md`. Auto skills go in every system prompt;
//! the others are injected into a message when the user writes `$name`.

use crate::agents::Scope;
use crate::config;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub scope: Scope,
    /// Frontmatter `auto: true`; `[skills] auto` in settings also turns a skill on.
    pub auto: bool,
    pub body: String,
}

fn parse(text: &str, fallback: &str, scope: Scope) -> Skill {
    let mut s = Skill { name: fallback.to_string(), description: String::new(), scope, auto: false, body: text.trim().to_string() };
    let Some((front, body)) = text.strip_prefix("---").and_then(|r| r.split_once("\n---")) else { return s };
    s.body = body.trim_start_matches(['-', '\n', '\r']).trim().to_string();
    for line in front.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim().trim_matches('"');
        match k.trim() {
            "name" if !v.is_empty() => s.name = v.to_string(),
            "description" => s.description = v.to_string(),
            "auto" => s.auto = v == "true",
            _ => {}
        }
    }
    s
}

fn load_dir(dir: &Path, scope: Scope, out: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        let (file, stem) = if p.is_dir() {
            (p.join("SKILL.md"), p.file_name().unwrap().to_string_lossy().to_string())
        } else if p.extension().is_some_and(|x| x == "md") {
            (p.clone(), p.file_stem().unwrap().to_string_lossy().to_string())
        } else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let s = parse(&text, &stem, scope);
        out.retain(|x| !x.name.eq_ignore_ascii_case(&s.name)); // local shadows global
        out.push(s);
    }
}

pub fn discover(project: &Path, auto: &[String]) -> Vec<Skill> {
    let mut out = Vec::new();
    load_dir(&config::home().join("skills"), Scope::Global, &mut out);
    load_dir(&project.join(".theta/skills"), Scope::Local, &mut out);
    for s in &mut out {
        s.auto |= auto.iter().any(|a| a.eq_ignore_ascii_case(&s.name));
    }
    out
}

fn block(s: &Skill) -> String {
    format!("<skill name=\"{}\">\n{}\n</skill>", s.name, s.body)
}

/// System-prompt section holding every auto skill ("" when none).
pub fn auto_section(skills: &[Skill]) -> String {
    let b: Vec<_> = skills.iter().filter(|s| s.auto).map(block).collect();
    if b.is_empty() { String::new() } else { format!("# Skills (always follow)\n{}", b.join("\n\n")) }
}

/// Prefix `text` with the body of each non-auto skill it mentions as `$name`.
pub fn expand(skills: &[Skill], text: &str) -> String {
    let mut used: Vec<&Skill> = vec![];
    for w in text.split_whitespace() {
        let n = w.strip_prefix('$').unwrap_or("").trim_end_matches(|c: char| !c.is_alphanumeric());
        if let Some(s) = skills.iter().find(|s| !s.auto && s.name.eq_ignore_ascii_case(n))
            && !used.iter().any(|u| u.name == s.name)
        {
            used.push(s);
        }
    }
    if used.is_empty() {
        return text.to_string();
    }
    format!("{}\n\n{text}", used.iter().map(|s| block(s)).collect::<Vec<_>>().join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_and_expand() {
        let a = parse("---\nname: caveman\nauto: true\n---\nBe terse.", "x", Scope::Global);
        let d = parse("---\ndescription: ui\n---\nUse grids.", "design", Scope::Local);
        let all = [a, d];
        assert_eq!(auto_section(&all), "# Skills (always follow)\n<skill name=\"caveman\">\nBe terse.\n</skill>");
        assert_eq!(expand(&all, "fix $design, costs $5 $caveman"), "<skill name=\"design\">\nUse grids.\n</skill>\n\nfix $design, costs $5 $caveman");
        assert_eq!(expand(&all, "plain"), "plain");
    }
}
