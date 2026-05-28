# Imported Hermes Skills

Snapshot of the skill catalog imported from Hermes into the v4 Evy
worktree as part of Phase 5 Slice — Skills Pull.

## Source

**Verified Hermes skill root:** `~/.hermes/skills/` (resolves from
`hermes_constants.get_skills_dir()` → `get_hermes_home()/skills`,
which honours `$HERMES_HOME` and falls back to `~/.hermes`).

The Hermes loader (`agent/skill_utils.py::iter_skill_index_files`)
walks the tree recursively with `os.walk` and excludes VCS / cache /
virtualenv directories. Hermes nests skills under category subdirs
(`<category>/<skill>/SKILL.md`); a handful sit at the top level
(`dogfood/SKILL.md`, `yuanbao/SKILL.md`).

The repo copy at `/Users/sem/code/hermes-agent/skills/` is slightly
divergent from the operator's runtime tree (4 skills the runtime has
but the repo does not; 3 the repo has but the runtime does not). The
runtime tree is the authoritative source for what Hermes actually
loads, so it is what we imported here.

## Destination

`<workspace_root>/skills/<skill_name>/SKILL.md` (+ any asset bundles
like `references/`, `templates/`). This is the in-tree catalog used
by the test harness and shipped with the repo.

Hermes's nested layout is flattened on import: `evy-skills`'s
`SkillRegistry::load` only walks `<dir>/*/SKILL.md` (one level), so
each category subdirectory contributes its skills directly under
`skills/`. No name collisions were found (`find … | awk '$(NF-1)' |
sort -u` confirmed all 94 directory names are unique).

For production deployment, `install.sh` (the parallel installer-
builder worker) will lift these into `~/.config/subctl/skills/` —
the path `SkillsConfig::directory` resolves at runtime.

## Config wiring

Added `SkillsConfig { directory: PathBuf, enabled: bool }` to
`crates/evy/src/config.rs` (`Config::skills`, `#[serde(default)]`).

Default: `directory = "skills"`, `enabled = true` — relative to the
daemon's working directory, so a checkout of the repo boots with the
in-tree catalog without further config.

## Totals

- **Skills imported:** 94
- **Skills skipped:** 0

The catalog crossed `gray_matter`'s YAML frontmatter parser without
errors (verified by `cargo test -p evy-skills` —
`imported_catalog::imported_catalog_loads_and_contains_required_skills`
loads all 94 and finds the operator-critical handful by name).

## Inventory

- `agent-first-web-avl-review` — Review, position, and polish Agent View Layer (AVL) / agent-first web projects: .agent pages, agent.txt manifests, AI-native rendering, MCP/API positioning, and marketing/category narrative.
- `airtable` — Airtable REST API via curl. Records CRUD, filters, upserts.
- `apple-notes` — Manage Apple Notes via memo CLI: create, search, edit.
- `apple-reminders` — Apple Reminders via remindctl: add, list, complete.
- `architecture-diagram` — Dark-themed SVG architecture/cloud/infra diagrams as HTML.
- `argentos-positioning-and-website-rewrite` — Reconstruct, critique, and rewrite ArgentOS.ai positioning and website copy using Hermes's external-auditor role, the user's Medium/Substack scar-tissue articles, and stored Obsidian source-of-truth documents. Use when working on ArgentOS messaging, homepage copy, Core vs Business positioning, ecosystem framing, identity/philosophy pages, or claims/trust language.
- `arxiv` — Search arXiv papers by keyword, author, category, or ID.
- `ascii-art` — ASCII art: pyfiglet, cowsay, boxes, image-to-ascii.
- `ascii-video` — ASCII video: convert video/audio to colored ASCII MP4/GIF.
- `audiocraft` — AudioCraft: MusicGen text-to-music, AudioGen text-to-sound.
- `axolotl` — Expert guidance for fine-tuning LLMs with Axolotl - YAML configs, 100+ models, LoRA/QLoRA, DPO/KTO/ORPO/GRPO, multimodal support
- `baoyu-comic` — Knowledge comics (知识漫画): educational, biography, tutorial.
- `baoyu-infographic` — Infographics: 21 layouts x 21 styles (信息图, 可视化).
- `blogwatcher` — Monitor blogs and RSS/Atom feeds for updates using the blogwatcher-cli tool. Add blogs, scan for new articles, track read status, and filter by category.
- `claude-code` — Delegate coding to Claude Code CLI (features, PRs).
- `claude-design` — Design one-off HTML artifacts (landing, deck, prototype).
- `codebase-inspection` — Inspect codebases w/ pygount: LOC, languages, ratios.
- `codex` — Delegate coding to OpenAI Codex CLI (features, PRs).
- `comfyui` — Generate images, video, and audio with ComfyUI — install, launch, manage nodes/models, run workflows with parameter injection. Uses the official comfy-cli for lifecycle and direct REST/WebSocket API for execution.
- `creative-ideation` — Generate project ideas via creative constraints.
- `debugging-hermes-tui-commands` — Debug Hermes TUI slash commands: Python, gateway, Ink UI.
- `design-md` — Author/validate/export Google's DESIGN.md token spec files.
- `dogfood` — Exploratory QA of web apps: find bugs, evidence, reports.
- `dspy` — DSPy: declarative LM programs, auto-optimize prompts, RAG.
- `excalidraw` — Hand-drawn Excalidraw JSON diagrams (arch, flow, seq).
- `findmy` — Track Apple devices/AirTags via FindMy.app on macOS.
- `gif-search` — Search/download GIFs from Tenor via curl + jq.
- `github-auth` — GitHub auth setup: HTTPS tokens, SSH keys, gh CLI login.
- `github-code-review` — Review PRs: diffs, inline comments via gh or REST.
- `github-issues` — Create, triage, label, assign GitHub issues via gh or REST.
- `github-pr-workflow` — Full pull request lifecycle — create branches, commit changes, open PRs, monitor CI status, auto-fix failures, and merge. Works with gh CLI or falls back to git + GitHub REST API via curl.
- `github-repo-management` — Clone/create/fork repos; manage remotes, releases.
- `godmode` — Jailbreak LLMs: Parseltongue, GODMODE, ULTRAPLINIAN.
- `google-workspace` — Gmail, Calendar, Drive, Docs, Sheets via gws CLI or Python.
- `heartmula` — HeartMuLa: Suno-like song generation from lyrics + tags.
- `hermes-agent-skill-authoring` — Author in-repo SKILL.md: frontmatter, validator, structure.
- `hermes-agent` — Complete guide to using and extending Hermes Agent — CLI usage, setup, configuration, spawning additional agents, gateway platforms, skills, voice, tools, profiles, and a concise contributor reference. Load this skill when helping users configure Hermes, troubleshoot issues, spawn agent instances, or make code contributions.
- `himalaya` — Himalaya CLI: IMAP/SMTP email from terminal.
- `huggingface-hub` — HuggingFace hf CLI: search/download/upload models, datasets.
- `humanizer` — Humanize text: strip AI-isms and add real voice.
- `imessage` — Send and receive iMessages/SMS via the imsg CLI on macOS.
- `jupyter-live-kernel` — Iterative Python via live Jupyter kernel (hamelnb).
- `kanban-orchestrator` — Decomposition playbook + anti-temptation rules for an orchestrator profile routing work through Kanban. The "don't do the work yourself" rule and the basic lifecycle are auto-injected into every kanban worker's system prompt; this skill is the deeper playbook when you're specifically playing the orchestrator role.
- `kanban-worker` — Pitfalls, examples, and edge cases for Hermes Kanban workers. The lifecycle itself is auto-injected into every worker's system prompt as KANBAN_GUIDANCE (from agent/prompt_builder.py); this skill is what you load when you want deeper detail on specific scenarios.
- `linear` — Linear: manage issues, projects, teams via GraphQL + curl.
- `llama-cpp` — llama.cpp local GGUF inference + HF Hub model discovery.
- `llm-wiki` — Karpathy's LLM Wiki: build/query interlinked markdown KB.
- `lm-evaluation-harness` — lm-eval-harness: benchmark LLMs (MMLU, GSM8K, etc.).
- `macos-computer-use` — macOS computer-use skill (multiline YAML description)
- `manim-video` — Manim CE animations: 3Blue1Brown math/algo videos.
- `maps` — Geocode, POIs, routes, timezones via OpenStreetMap/OSRM.
- `minecraft-modpack-server` — Host modded Minecraft servers (CurseForge, Modrinth).
- `nano-pdf` — Edit PDF text/typos/titles via nano-pdf CLI (NL prompts).
- `native-mcp` — MCP client: connect servers, register tools (stdio/HTTP).
- `node-inspect-debugger` — Debug Node.js via --inspect + Chrome DevTools Protocol CLI.
- `notion` — Notion API via curl: pages, databases, blocks, search.
- `obliteratus` — OBLITERATUS: abliterate LLM refusals (diff-in-means).
- `obsidian` — Read, search, create, and edit notes in the Obsidian vault.
- `ocr-and-documents` — Extract text from PDFs/scans (pymupdf, marker-pdf).
- `opencode` — Delegate coding to OpenCode CLI (features, PR review).
- `openhue` — Control Philips Hue lights, scenes, rooms via OpenHue CLI.
- `outlines` — Guarantee valid JSON/XML/code structure during generation, use Pydantic models for type-safe outputs, support local models (Transformers, vLLM), and maximize inference speed with Outlines - dottxt.ai's structured generation library
- `p5js` — p5.js sketches: gen art, shaders, interactive, 3D.
- `pixel-art` — Pixel art w/ era palettes (NES, Game Boy, PICO-8).
- `plan` — Plan mode: write markdown plan to .hermes/plans/, no exec.
- `pokemon-player` — Play Pokemon via headless emulator + RAM reads.
- `polymarket` — Query Polymarket: markets, prices, orderbooks, history.
- `popular-web-designs` — 54 real design systems (Stripe, Linear, Vercel) as HTML/CSS.
- `powerpoint` — Create, read, edit .pptx decks, slides, notes, templates.
- `pretext` — Use when building creative browser demos with @chenglou/pretext — DOM-free text layout for ASCII art, typographic flow around obstacles, text-as-geometry games, kinetic typography, and text-powered generative art. Produces single-file HTML demos by default.
- `project-credibility-review` — Evaluate a startup/open-source project or AI product by cross-checking the website narrative against repositories, docs, demos, activity, and implementation evidence. Use when the user asks what you think of a product/project URL, especially if it has GitHub links or open-source claims.
- `python-debugpy` — Debug Python: pdb REPL + debugpy remote (DAP).
- `requesting-code-review` — Pre-commit review: security scan, quality gates, auto-fix.
- `research-paper-writing` — Write ML papers for NeurIPS/ICML/ICLR: design→submit.
- `segment-anything` — SAM: zero-shot image segmentation via points, boxes, masks.
- `sketch` — Throwaway HTML mockups: 2-3 design variants to compare.
- `songsee` — Audio spectrograms/features (mel, chroma, MFCC) via CLI.
- `songwriting-and-ai-music` — Songwriting craft and Suno AI music prompts.
- `spike` — Throwaway experiments to validate an idea before build.
- `spotify` — Spotify: play, search, queue, manage playlists and devices.
- `subagent-driven-development` — Execute plans via delegate_task subagents (2-stage review).
- `systematic-debugging` — 4-phase root cause debugging: understand bugs before fixing.
- `teams-meeting-pipeline` — Operate the Teams meeting summary pipeline via Hermes CLI — summarize meetings, inspect pipeline status, replay jobs, manage Microsoft Graph subscriptions.
- `test-driven-development` — TDD: enforce RED-GREEN-REFACTOR, tests before code.
- `touchdesigner-mcp` — Control a running TouchDesigner instance via twozero MCP — create operators, set parameters, wire connections, execute Python, build real-time visuals. 36 native tools.
- `trl-fine-tuning` — Fine-tune LLMs using reinforcement learning with TRL - SFT for instruction tuning, DPO for preference alignment, PPO/GRPO for reward optimization, and reward model training. Use when need RLHF, align model with preferences, or train from human feedback. Works with HuggingFace Transformers.
- `unsloth` — Expert guidance for fast fine-tuning with Unsloth - 2-5x faster training, 50-80% less memory, LoRA/QLoRA optimization
- `vllm` — vLLM: high-throughput LLM serving, OpenAI API, quantization.
- `webhook-subscriptions` — Webhook subscriptions: event-driven agent runs.
- `weights-and-biases` — W&B: log ML experiments, sweeps, model registry, dashboards.
- `writing-plans` — Write implementation plans: bite-sized tasks, paths, code.
- `xurl` — X/Twitter via xurl CLI: post, search, DM, media, v2 API.
- `youtube-content` — YouTube transcripts to summaries, threads, blogs.
- `yuanbao` — Yuanbao (元宝) groups: @mention users, query info/members.
