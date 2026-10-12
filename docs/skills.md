# Skills

A skill is a markdown rule file. Global: `~/.theta/skills/`. Project: `<project>/.theta/skills/`. Project wins on the same name.

## Format

Either `<name>.md` or `<name>/SKILL.md`:

```markdown
---
name: caveman
description: terse answers
auto: false
---
Answer in short sentences. No filler.
```

- `name`: optional, defaults to the file or folder name.
- `description`: one line.
- `auto: true`: injected into every system prompt.

## Use

- `$name` in a message injects that skill for this message only.
- `[skills] auto = ["name"]` in settings also makes it auto.
- `[skills] subagent = [...]`: injected into every subagent. Default `["caveman", "ponytail"]`; absent names are ignored.
- `/skills`: list skills, `*` marks auto ones, with scope (global or local).

## Reload

After adding or editing a skill: `/reload`.
