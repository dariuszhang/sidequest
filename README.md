# SideQuest

Turn idle coding-agent budget into overnight magic.

SideQuest is a Rust CLI and background daemon that watches your Claude Code and Codex usage, waits for your sleep window, and spends only the budget from the 5 hour session that would otherwise expire before morning. It autonomously refines and builds on top of your existing repos, and incrementally builds your side project for you while you sleep.

SideQuest always leaves you with a fresh 100% session in the morning, enforced by a configurable safety margin and continuous budget monitoring.

Wake up. Run `sidequest loot`. Cherry-pick the wins. 


## How it works

```
 You sleep         SideQuest wakes up        You wake up
─────────┐        ┌──────────────────┐       ┌──────────
 zzz...  │───────▶│ Checks remaining │──────▶│ sidequest loot
         │        │ budget, spawns   │       │ Review & cherry-pick
         │        │ agents on your   │       │ results on isolated
         │        │ repo targets     │       │ git branches
─────────┘        └──────────────────┘       └──────────
```

SideQuest never touches your working branches. Every run lands on an isolated branch (`sidequest/grind/<name>` or `sidequest/quest/<name>`) so you stay in full control.

SideQuest operates in two modes:

### 1. Grind — "Your code, but better"

Grind mode targets your existing repos. SideQuest reads your recent git history and your entire codebase, then improves what's there — without you asking for anything specific.

It catches bugs you introduced: race conditions, null pointer paths, off-by-one errors. It adds test coverage for untested code paths. It extracts repeated patterns into shared utilities. It fixes security issues flagged by pattern matching against known vulnerability classes. It cleans up the TODOs you left behind.

But it goes beyond fixing what you committed. It reads your codebase and looks for structural opportunities — noticing manual processes you keep doing (formatting, validation, error handling) and building utilities. It spots missing infrastructure: a Dockerfile you keep rewriting, a CI config you've been meaning to add, documentation for an undocumented API. It infers the shape of a missing tool from the friction patterns in your code.

Each improvement lands in a self-contained git branch with passing tests. Nothing touches `main` until you review and merge.


### 2. Quest — "Grand goals, incremental progress"

Quests are the ambitious mode. You give SideQuest a goal — a direction that you haven't explored, or a side project that you haven't got the time to start:

*"Build me a CLI tool that helps me manage my files across machines."*

*"Create a personal API that aggregates my calendar, todos, and weather into a single morning dashboard endpoint."*

*"Set up a monitoring stack for my side project with alerts when the API goes down."*

SideQuest works on these incrementally, one night at a time, spending whatever tokens remain after Grind has taken its share. Each morning, you get a progress report: what was accomplished, what's next, and the current state of the project. The quest lives in its own repository or branch.

Some nights, SideQuest makes big progress. Some nights, it only has enough tokens for a small step. Some nights, it has nothing to spare and the quest pauses. That's fine. Over days and weeks, the project accumulates. One morning, you wake up to a working first version.

## Install

### Option 1: Homebrew (recommended on macOS)

```bash
brew tap dariuszhang/tap
brew install sidequest
```

### Option 2: Download prebuilt binary (GitHub Releases)

1. Open the latest release: <https://github.com/dariuszhang/sidequest/releases/latest>
2. Download the archive for your machine:
   - Apple Silicon (M1/M2/M3): `aarch64-apple-darwin`
   - Intel Mac: `x86_64-apple-darwin`
3. Extract and move `sidequest` into your `PATH` (for example `/usr/local/bin` or `/opt/homebrew/bin`).

### Option 3: Build from source (Cargo)

```bash
cargo install --git https://github.com/dariuszhang/sidequest.git sidequest
```

Then run initial setup:

```bash
sidequest init
```

## Quick start

### 1. Register repos for nightly grinding

Point SideQuest at the repos you want maintained overnight:

```bash
sidequest grind /path/to/my-project
sidequest grind /path/to/another-repo
```

Or let it scan for repos automatically:

```bash
sidequest grind scan
```

### 2. Define a quest (optional)

Quests are open-ended goals described in plain English. SideQuest creates a fresh project directory and works toward the goal autonomously:

```bash
sidequest quest "Build a CLI tool that converts markdown tables to CSV"
```

You can also point to a file with a longer description:

```bash
sidequest quest goal_file:specs/new-feature.md
```

### 3. Let it run

SideQuest runs as a background daemon that activates during your configured sleep window:

```bash
sidequest install   # Set up autostart (launchd on macOS, systemd on Linux)
```

For immediate local testing without waiting for the sleep window:

```bash
sidequest run --now
```

### 4. Review results in the morning

```bash
sidequest loot
```

The interactive loot review lets you inspect diffs, accept or reject changes, and cherry-pick results branch by branch.

## Daily workflow


| Command               | What it does                                                 |
| --------------------- | ------------------------------------------------------------ |
| `sidequest`           | Print status overview — budget, upcoming window, queued work |
| `sidequest loot`      | Interactive morning review of overnight results              |
| `sidequest run --now` | Trigger a run immediately (skips sleep-window check)         |
| `sidequest stop`      | Halt a running session                                       |


## Managing work

**Grind** targets are existing repos. SideQuest delegates maintenance-style work (refactors, test coverage, TODOs, lint fixes) to an agent on an isolated branch.

```bash
sidequest grind list                # Show registered repos
sidequest grind remove my-project   # Remove a target by name or path
sidequest grind scan                # Discover and toggle repos interactively
```

**Quest** targets are goal-driven. You describe what you want built and SideQuest creates a project directory, spawns an agent, and iterates toward the goal across multiple nights.

```bash
sidequest quest list                # Show all quests
sidequest quest edit my-quest       # Edit goal text
sidequest quest pause my-quest      # Pause a quest
sidequest quest resume my-quest     # Resume a paused quest
sidequest quest log my-quest        # View session logs
sidequest quest remove my-quest     # Delete a quest
```

## Configuration

After `sidequest init`, your config lives at `~/.sidequest/config.yaml`. Key settings:

```yaml
# Sleep window — when SideQuest is allowed to run
sleep_start: "23:00"
sleep_end: "07:00"

# Provider — which AI coding agent to use
provider: claude  # or codex

# Ordering — run quests before grind work
prefer_quests: true

# Where new quest projects are created
quest_projects_directory: ~/projects/sidequest-quests
```

Prompt behavior is configurable under `prompts.work_delegation`, `prompts.actions.grind`, and `prompts.actions.quest` for users who want fine-grained control over how agents are instructed.

Set `SIDEQUEST_HOME=/some/path` to use an alternate state directory (useful for development or testing).

## Architecture

SideQuest is structured around clear module boundaries:

**Scheduler** (`src/scheduler.rs`) — Deterministic budget math. Computes your sleep window, calculates how much budget is available to spend before morning, and determines cutoff times. The scheduler is pure and fully unit-tested — no side effects, no network calls.

**Oracle** (`src/oracle/`) — Provider-specific usage detection. Probes Claude and Codex APIs to determine current consumption and remaining budget. Each provider implements a common trait so adding new providers is straightforward.

**Spawner** (`src/spawner.rs`) — One-agent-at-a-time child process execution. Manages agent lifecycles with watchdog timers and graceful shutdown. Only one agent runs at a time to stay within budget constraints.

**Harvester** (`src/harvester.rs`) — Git branch management. Creates isolated branches per target, handles branch refresh, and generates loot banners summarizing what changed.

**Daemon** (`src/daemon.rs`) — Session orchestrator. Runs the recurring loop: check the schedule, poll the oracle, pick the next work item, spawn an agent, harvest results.

**Platform** (`src/platform/`) — OS abstractions behind a trait. Handles autostart registration (launchd / systemd), desktop notifications, and shell hooks for macOS and Linux.

**Runtime** (`src/runtime.rs`) — File-backed IPC under `~/.sidequest/state/`. The daemon writes runtime snapshots and events; the CLI reads them for status display. No sockets, no database — just JSON files.

```
┌─────────────────────────────────────────────┐
│                   CLI                       │
│  init · grind · quest · loot · run · stop   │
└────────────────────┬────────────────────────┘
                     │
          ┌──────────▼──────────┐
          │      Daemon         │
          │  schedule → oracle  │
          │  → spawn → harvest  │
          └──────────┬──────────┘
                     │
    ┌────────┬───────┼───────┬──────────┐
    ▼        ▼       ▼       ▼          ▼
Scheduler  Oracle  Spawner  Harvester  Platform
```

## State layout

All runtime state lives under `~/.sidequest/` by default:

```
~/.sidequest/
├── config.yaml                    # User configuration
├── harvests/                      # Collected overnight results
├── logs/                          # Session logs
├── quests/                        # Quest project directories
└── state/
    ├── backlog.json               # Queued work items
    ├── control-requests.jsonl     # CLI → daemon commands
    ├── harvest-ledger.json        # Record of harvested results
    ├── last-session-report.json   # Most recent session summary
    ├── quests/                    # Per-quest state
    ├── runtime-events.jsonl       # Event stream
    └── runtime-snapshot.json      # Current daemon state
```

## Support & Roadmap
Currently, SideQuest is only available for MacOS. The plan is to extend support to Windows and Linux machines, and extend AI service providers beyond Claude Code and Codex.
