# ways of working
- Always make surgical small changes and reuse what exists
- Always act as the very senior developer that reaches for the easy solution
- Always use existing tooling before implementing new
- Always ensure tests run without errors
- Always ensure a clean subagent runs code review without major findings before considering being done

# Agent delegation and accountability
- The primary agent is the senior lead and is solely accountable for requirements, planning, architecture, delegation, integration, verification, and the final handoff.
- Use only Luna subagents. Delegate exploration, scouting, lookups, website inspection, and log reading to Luna at high effort.
- Delegate every code-implementation and file-editing task to a Luna subagent at maximum effort; the primary agent must not implement code or edit files directly.
- The primary agent must critically verify delegated work and run the relevant tests before handoff.
- Before declaring work complete, assign a fresh, independent, clean Luna subagent at maximum effort to review the final diff, and resolve every major finding.

- Always enumerate every existing UI surface a requested status/indicator could live on (e.g. sidebar item list, subscribed-PR feed, cmd-k picker, content-pane titlebar) before implementing it — don't default to the first plausible one found.
- Always confirm the exact visual treatment (icon vs. text/pill, which icon, where in the row) for a new UI indicator before writing code, when the ask doesn't spell it out.
- If placement or UI request is unclear, ask for a screenshot as a visual aid.
