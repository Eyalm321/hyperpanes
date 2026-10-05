# goal-orchestrator personas (bundled snapshot)

These three files are the agent personas for the goals system (see
`docs/goals-system-plan.md`): `SKILL.md` (goals orchestrator), `SPEC.md` (per-goal spec
agent), `IMPL.md` (per-subtask impl agent). The app spawns a `claude` pane with
`--append-system-prompt-file <this dir>/SKILL.md` when you create a goal, and the persona
references `SPEC.md`/`IMPL.md` beside it.

**Source of truth is the skills repo** —
`agent-orchestration-skills/skills/goal-orchestrator/` (github.com/Eyalm321/agent-orchestration-skills).
This copy is a **bundled snapshot** so the installed app resolves the persona without the
skills repo present (`State::submit_new_goal` looks here, under `resources/claude/`, via the
same exe-relative / FHS layouts as the shell-integration scripts). When the personas change
upstream, re-copy the three `.md` files here and commit — do not edit this copy in isolation.

**CI keeps the two in step.** `scripts/check-persona-sync.sh` byte-compares `SKILL.md`,
`SPEC.md` and `IMPL.md` here against the skills repo's `main`. It gates any PR that touches this
directory (the `persona-sync` job in `verify.yml`) and runs daily on its own
(`persona-sync.yml`), so an edit made to only one copy turns a check red instead of drifting
silently. Edit both copies together. This README is excluded, since it describes the snapshot itself.
