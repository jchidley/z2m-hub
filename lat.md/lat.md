# z2m-hub

z2m-hub is a single-binary LAN automation hub for Zigbee lighting, DHW tracking, and a mobile dashboard.

## Knowledge map

This graph is the source of truth for current architecture, domain rules, and live constraints.

- [[architecture]] explains the runtime shape and shared-state model.
- [[automations]] captures motion-light behaviour and override rules.
- [[dhw]] explains the remaining-hot-water model.
- [[interfaces]] defines HTTP, WebSocket, eBUS, PostgreSQL, and heating proxy contracts.
- [[infrastructure]] records hosts, config ownership, and deployment shape.
- [[constraints]] captures hard boundaries that should guide code changes.
- [[tests]] records durable test specs and `@lat:` traceability anchors for high-value invariants.
- [[tsdb-migration]] records the remaining shared completion work and repo-local closeout evidence for the InfluxDB-to-PostgreSQL migration.

## Reading order

Start with architecture, then jump to the subsystem or interface that matches the task.

For most work, read [[architecture#Runtime structure]], then one of [[automations#Motion lighting automation]], [[dhw#DHW tracking model]], [[interfaces#External interfaces]], or [[tests]] when you are auditing or strengthening coverage.

## Adjacent docs

Human-facing narrative stays outside this graph so each fact has one obvious home.

- `README.md` is for signposting, build/deploy basics, and project framing.
- `AGENTS.md` is for agent workflow, commands, and short operational gotchas.
