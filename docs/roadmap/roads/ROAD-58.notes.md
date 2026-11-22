# ROAD-58 agent-scripted-visuals — notes

Split from ROAD-44 on 2026-11-22, while framing ROAD-33.

The owner wanted inline visuals "hoàn thiện nhất". Asked whether that means static images an agent posts or scripts an agent writes and Reno runs, they chose to do both: static visuals in ROAD-33, and scripted visuals as a road of their own.

A ruling from 2026-11-19 keeps scripted, agent-written scenes out: Reno renders harness output, not agent-written UI. This road reverses that ruling, so before any build it needs:
- the vision edited, through define-project;
- an ADR on running agent-written code inside the app: the sandbox runtime, the limits, and what a script may reach.
