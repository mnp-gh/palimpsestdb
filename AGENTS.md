## Agent skills

### Issue tracker

Issues and specs live as local markdown files under `.scratch/<feature-slug>/`. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`, recorded as a `Status:` line in each issue file. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.

### Skills

Skills (Matt Pocock's and ponytail) live in `.agents/skills/<name>/SKILL.md`; load the matching one when a task fits its description.

On coding tasks, follow `.agents/skills/ponytail/SKILL.md` (smallest complete change). Reviews and audits: `.agents/skills/ponytail-review/` and `.agents/skills/ponytail-audit/`.

## Project docs

Writing or reviewing code: follow `docs/CONVENTIONS.md`. Planning or scoping work: `docs/ROADMAP.md` defines what each version includes.
