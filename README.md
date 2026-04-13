# z2m-hub

Rust server for home automation. Connects to Zigbee2MQTT via WebSocket and a Vaillant heat pump via eBUS. Serves a mobile dashboard for controlling lights and hot water.

## Quick Start

```bash
# Build for Raspberry Pi 5
cargo build --release --target aarch64-unknown-linux-gnu

# Deploy
scp target/aarch64-unknown-linux-gnu/release/z2m-hub jack@pi5data:/tmp/z2m-hub
ssh jack@pi5data 'sudo mv /tmp/z2m-hub /usr/local/bin/z2m-hub && sudo systemctl restart z2m-hub'

# Dashboard
open http://10.0.1.230:3030
```

## What It Does

- **Motion → lights**: Two Aqara motion sensors trigger hall and landing lights when dark enough.
- **Hot water gauge**: A physics-based DHW model estimates remaining usable hot water.
- **DHW boost**: One tap requests a charge cycle from the heat pump via eBUS.
- **Light toggles**: The dashboard controls hall, landing, and top landing switches.
- **Heating proxy**: The dashboard forwards heating mode controls to a separate LAN service.

## Documentation

- [`lat.md/`](lat.md/) — canonical current-state knowledge graph for architecture, domain rules, interfaces, infrastructure, and constraints
  - [Graph entrypoint](lat.md/lat.md)
  - [Runtime structure](lat.md/architecture.md)
  - [Automations](lat.md/automations.md)
  - [DHW tracking model](lat.md/dhw.md)
  - [External interfaces](lat.md/interfaces.md)
  - [Deployment and configuration](lat.md/infrastructure.md)
  - [Operational constraints](lat.md/constraints.md)
- [`AGENTS.md`](AGENTS.md) — agent workflow, commands, and repo-specific execution rules

When docs and code disagree, code wins. When multiple docs could carry the same current-state fact, prefer `lat.md/` as the authoritative home.

## About This Code

Almost all of this code is AI/LLM-generated. It's best used as a source of
inspiration for your own AI/LLM efforts rather than as a traditional library.

**This is personal alpha software.** All my GitHub projects should be considered
experimental. If you want to use them:

- **Pin to a specific commit** — don't track `main`, it changes without warning
- **Use AI/LLM to adapt** — without AI assistance, these projects are hard to use
- **Treat as inspiration** — build your own version rather than depending on mine

**Suggestions welcome** — If you have ideas for improvements or changes, I'd be
delighted to read them and use them as inspiration for my own efforts.

**Why not a library?** These days it's often quicker to use AI/LLM to build your
own than to integrate traditional libraries. My use of AI/LLM is inspired by
these people and posts:

- [Simon Willison's Weblog](https://simonwillison.net/) — Essential reading on
  LLMs, prompt engineering, and building with AI
- [CLI over MCP](https://lucumr.pocoo.org/2025/8/18/code-mcps/) — Armin Ronacher
  on why command-line tools are better integration points than custom protocols
- [Build It Yourself](https://lucumr.pocoo.org/2025/12/22/a-year-of-vibes/) —
  Armin Ronacher: "With our newfound power from agentic coding tools, you can
  build much of this yourself..."
- [Shipping at Inference Speed](https://steipete.me/posts/2025/shipping-at-inference-speed) —
  Peter Steinberger on the new workflow of building with AI assistance
- [Year in Review 2025](https://mariozechner.at/posts/2025-12-22-year-in-review-2025/) —
  Mario Zechner on AI-assisted development

**What I use:** Currently Anthropic's Claude Opus, evaluating OpenAI's GPT Codex
as an alternative.

## License

This project is dual-licensed under the terms of both the MIT license and the
Apache License (Version 2.0).

See [LICENSE-APACHE](LICENSE-APACHE) and [LICENSE-MIT](LICENSE-MIT) for details.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
