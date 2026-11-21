# Adapt

Adapt is a Rust terminal coding agent for macOS and Linux. It uses Adaption's Responses API to explore a project, propose edits, run developer commands, and stream replies.

## Install and run

You need Git, [Rust and Cargo](https://rust-lang.org/tools/install/) 1.88 or later, and a C compiler and linker. On macOS, install Apple's Command Line Tools with `xcode-select --install` if needed. On Debian or Ubuntu, install the `build-essential` package.

Clone the repository and install the `adapt` command:

```sh
git clone https://github.com/James-Software/Adaption-Labs-Harness.git
cd Adaption-Labs-Harness
cargo install --locked --path .
adapt --version
```

If a Rustup installation can't find `cargo` or `adapt`, open a new terminal or run `source "$HOME/.cargo/env"` in Bash or Zsh, then retry.

Create your own [Adaption API key](https://docs.adaptionlabs.ai/introduction/create-api-keys/) in **Settings → API keys**. Replace the path below with the project you want Adapt to work on:

```sh
adapt --no-memory -C /path/to/your/project
```

1. In the first-run menu, press **Enter** to open `/settings`.
2. Choose **1. API key**, paste the key into the hidden prompt, and press **Enter**.
3. Choose **5. Save and return**.
4. At `adapt>`, try `List the project files and summarize the README.` Type `/quit` to leave.

Run `adapt models` to check that your key can list the available models. The default model is `qwen3.5-397b`. Open `/settings` again to change your key or model. Saved keys are plaintext in `~/.adaption-code/settings.json` with owner-only permissions. For session-only configuration, set `ADAPTION_API_KEY` in your environment.

When you launch without `-C`, Adapt uses the Git root if it finds one, or the current directory otherwise. For one-off prompts, run `adapt -C /path/to/your/project ask "Summarize the README."` or pipe a multiline prompt to `adapt ask -`. The `ADAPTION_MODEL` environment variable and `--model` flag override the saved model.

Type `quit`, `exit`, or `/quit` at a prompt to leave. Ctrl+C exits while waiting for input and cancels an active coding task before returning to the prompt. Set `NO_COLOR=1` for plain terminal output.

## Hackathon use

Share the source checkout and have participants use their own Adaption API keys. Run `adapt --no-memory -C /path/to/hackathon-project` in a clean project copy with files you can send to Adaption. The model can read files anywhere inside the selected workspace, subject to the built-in path checks. Ignore patterns affect listing and search. Direct reads of explicitly named files use the path checks and can include ignored files.

Review the full diff or command and prefer **Allow This Time**. Approved commands have your account's filesystem and network access. Use an isolated development environment for unfamiliar code, scripts, or dependencies. Keep credentials and private project files out of that environment.

## Use an adapting model

In Adaption's app, create an active inference stack with your preferred coding model. The key must belong to the stack's organization, and the caller must own the stack. In `/settings`, set **Model or stack** to `stack:my-coder` using your stack's slug, and set **Agentic memory** to **On** or **Stack default**.

Agentic memory lets Adaption build and recall information about your work, decisions, and preferences across conversations. It is scoped to the user and stack, and new memories are built in the background. You can manage them in the Adaption app. The CLI flags `--memory` and `--no-memory` override the saved memory preference for one session. The CLI explicitly disables stack self-improvement on its requests. See [Inference stack memory](https://docs.adaptionlabs.ai/inference-stacks/key-concepts/#memory).

Prompts, source text returned by file tools, and tool results, including captured command stdout and stderr, are sent to Adaption for inference. The CLI keeps conversation history in process memory, replays it with `store: false`, and drops older turns when the context limit is reached. The `store` flag controls retaining responses for future requests by response ID. Adaption's stack memory is a separate setting. Explicit local function definitions are sent on each tool-enabled request. See [Adaption's Responses guide](https://docs.adaptionlabs.ai/inference-stacks/generate-a-model-response/) and [API reference](https://docs.adaptionlabs.ai/api/resources/responses/methods/create).

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
cargo build --locked
python3 tests/cli_smoke.py
```

The Rust tests exercise the Adaption request and streaming tool loop with a loopback mock server, approval decisions, command arguments and environment, settings permissions, workspace boundaries, and stale edits. The Python 3.9+ smoke tests run the built CLI in a real pseudo-terminal with temporary settings and a fake key. To check against your account, use `adapt models`, then ask the agent to list files in a demo project and summarize one.
