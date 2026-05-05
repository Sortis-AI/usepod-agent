# usepod-agent

Provider agent for the [Use Pod](https://usepod.ai) AI inference marketplace. Operators run this on their hardware to receive dispatched inference jobs from the Use Pod coordinator and serve them via a local backend (vLLM, llama.cpp, LM Studio, Ollama) or upstream proxy (OpenRouter, Venice).

## Install

```sh
curl -fsSL https://usepod.ai/install.sh | sh
usepod-agent --help
```

Linux x86_64/arm64, macOS Apple Silicon, and Windows x86_64 binaries are attached to each [GitHub Release](https://github.com/Sortis-AI/usepod-agent/releases) with SHA-256 sums.

## Configure

Copy `agent.example.toml` to `~/.config/usepod-agent/agent.toml` and fill in:

- Coordinator URL (default `wss://api.usepod.ai/provider/connect`)
- Operator wallet (Solana, where USDC payouts arrive)
- Per-model pricing
- Backend list

Then run:

```sh
usepod-agent run
```

The agent generates an Ed25519 identity key on first run (`~/.usepod-agent/identity.key`, mode `0600`), connects outbound over WSS, registers capabilities, and begins receiving jobs.

## Run as a system service

For production operators, install the agent as a managed service so it
starts on boot and restarts on crash. One subcommand, three platforms:

```sh
sudo usepod-agent service install   # writes the unit/plist/service entry
sudo usepod-agent service start
usepod-agent service status         # exits 3 if not installed
usepod-agent service logs -f
```

| Platform | Backend | Where it lives | Run-as | Logs |
|---|---|---|---|---|
| Linux | systemd | `/etc/systemd/system/usepod-agent.service` | dedicated `usepod` user (created on install) | `journalctl -u usepod-agent` |
| macOS | launchd | `/Library/LaunchDaemons/ai.usepod.agent.plist` | root | `/var/log/usepod-agent.log` |
| Windows | SCM | service `ai.usepod.agent` | LocalSystem | `%ProgramData%\usepod-agent\agent.log` |

`service install` propagates the surrounding `--config` and `--log-level`
flags (when non-default) into the generated service entry, so
`sudo usepod-agent --config /etc/usepod/agent.toml service install` does the
right thing. `service uninstall` reverses it. `service restart` is a
stop-then-start.

## Build from source

```sh
git clone https://github.com/Sortis-AI/usepod-agent
cd usepod-agent
cargo build --release
./target/release/usepod-agent --help
```

## Claude Code skill

A first-party Claude Code skill ships at `.claude/skills/usepod-agent-setup/`. If you run Claude Code on the host machine, it can install, pair, and service-ify the agent for you. To make it available globally:

```sh
mkdir -p ~/.claude/skills
cp -r .claude/skills/usepod-agent-setup ~/.claude/skills/
```

Then in any Claude Code session: "set up usepod-agent on this machine".

## Cross-repo

The Use Pod coordinator (the marketplace itself, USDC settlement, dashboard, SDKs) is developed in a separate, private monorepo. The wire protocol between agent and coordinator is documented in `src/ws_client.rs` (agent side) and mirrored on the coordinator side.

## License

Apache-2.0
