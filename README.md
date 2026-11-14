# Adaption Code

Adaption Code is a Rust terminal coding agent for macOS and Linux. It uses Adaption's Responses API to explore a project, propose edits, run developer commands, and stream replies.

## Install and run

Install [stable Rust](https://rust-lang.org/tools/install/) and a C linker, then run this from the harness checkout:

```sh
cargo install --locked --path .
```

Create an [Adaption API key](https://docs.adaptionlabs.ai/inference-stacks/quickstart/) and set `ADAPTION_API_KEY` in your shell or secret manager. Run these commands from the project you want to work on:

```sh
adaption-code models
adaption-code
adaption-code ask "Find where the CLI parses flags and explain how to add one."
```

The CLI uses the Git root when it finds one and the current directory otherwise. Use `-C /path/to/project` to select a different workspace. The default model is `qwen3.5-397b`. Set `ADAPTION_MODEL` or pass `--model` to use another ID from `adaption-code models`. Pipe a multiline prompt with `adaption-code ask -`. Interactive sessions support `/help`, `/clear`, `/permissions`, `/revoke`, and `/quit`.

## Use an adapting model

In Adaption's app, create an active inference stack with your preferred coding model. The key must belong to the stack's organization, and the caller must own the stack. Use its slug with:

```sh
adaption-code --model stack:my-coder --memory
```

Agentic memory lets Adaption build and recall information about your work, decisions, and preferences across conversations. It is scoped to the user and stack, and new memories are built in the background. You can manage them in the Adaption app. Omit `--memory` to use the stack's setting, or use `--no-memory` to disable memory for the session. This is adaptation through remembered context. Model training is managed separately in Adaption. See [Inference stack memory](https://docs.adaptionlabs.ai/inference-stacks/key-concepts/#memory).

Prompts, relevant source text, and tool results are sent to Adaption for inference. The CLI keeps conversation history in process memory, replays it with `store: false`, and drops older turns when the context limit is reached. Adaption's stack memory is a separate setting. Explicit local function definitions are sent on each tool-enabled request, which keeps the coding workflow on the CLI's own tools. See [Adaption's Responses guide](https://docs.adaptionlabs.ai/inference-stacks/generate-a-model-response/) and [API reference](https://docs.adaptionlabs.ai/api/resources/responses/methods/create).

## Approvals

The agent can list, search, and read project files without prompting. Search and listing honor ignore rules. Built-in file operations reject parent traversal, symlinks, and common credential paths. Writes show a diff and recheck the target after review.

- **1. Always Allow** remembers the exact action until you exit or use `/revoke`. A command must match the resolved executable and its fingerprint, argument list, and working directory. An edit must match the path and reviewed change.
- **2. Allow This Time** grants a single invocation.
- **3. Deny** skips the action. Enter alone, EOF, and the absence of a controlling terminal also deny.

Commands receive literal arguments and start in the displayed directory with your user account's permissions. They inherit your environment except for `ADAPTION_API_KEY`. A command can invoke project scripts and dependencies and may behave differently after those change. Use **Allow This Time** when you want to review each run. Approvals limit which action the harness starts. An approved process can still have side effects outside the project, and concurrent programs can still change files.

## Verify

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

The tests exercise the Adaption request and streaming tool loop with a local mock, approval matching, literal command arguments, path boundaries, and stale edits. To check against your account, use `adaption-code models`, then ask the agent to list a project's files and summarize one.
