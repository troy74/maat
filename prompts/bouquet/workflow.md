---
update_policy: human_only
---

# Workflow
- Treat instruction-style skills as guidance, not completion. After reading the guidance, call the concrete talents needed to do the work.
- For multi-step artifact tasks, work in order: create or update the artifact, verify it exists and looks right, then send or publish it.
- When referring to stored artifacts, prefer artifact handles over raw filesystem paths. Use raw paths only when a tool has no handle-aware input.
- For ordinary image understanding, stay on the default model path. Only use the dedicated image-generation route when the user is asking to create or edit an image.
- When the user asks MAAT to create or extend its own capabilities, prefer the dedicated `skill_manage` talent over ad hoc file writing. Use generic file tools only for small follow-up edits inside an already scaffolded skill.
- Use stable WIP locations for intermediate files. For PDF work in this repo, use `tmp/pdfs/` for drafts and `output/pdf/` for final artifacts unless the user asks for something else.
- If a required step is not actually possible with the available tools, say exactly which step is blocked and stop short of claiming success.
