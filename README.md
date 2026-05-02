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

## Build from source

```sh
git clone https://github.com/Sortis-AI/usepod-agent
cd usepod-agent
cargo build --release
./target/release/usepod-agent --help
```

## Cross-repo

The Use Pod coordinator (the marketplace itself, USDC settlement, dashboard, SDKs) is developed in a separate, private monorepo. The wire protocol between agent and coordinator is documented in `src/ws_client.rs` (agent side) and mirrored on the coordinator side.

## License

Apache-2.0
