---
name: usepod-agent-setup
description: Install, configure, and run usepod-agent — the Use Pod marketplace provider agent — on the current machine. Use when the user says "set up usepod-agent", "become a Use Pod provider", "host my GPU on Use Pod", "pair my agent", or asks to expose a local vLLM / llama.cpp / LM Studio / Ollama backend to the Use Pod marketplace.
---

# usepod-agent operator setup

You are setting up `usepod-agent` on the user's machine so they can earn USDC by serving inference jobs on the Use Pod marketplace. The agent is a long-lived outbound WSS client that proxies jobs from `api.usepod.ai` to a local OpenAI-compatible backend.

## Prerequisites — confirm before changing anything

Ask, do not assume, when any of these are unclear:

1. **Which backend** they intend to expose: vLLM (`:8000`), llama.cpp `llama-server` (`:8080`), LM Studio (`:1234`), or Ollama (`:11434`). If none is running, offer the from-scratch path (§5).
2. **Solana wallet address** (base58) where USDC payouts should land. The agent does not need this at setup time — it is entered in the dashboard during pair-claim — but the user should have it ready.
3. **Persistence mode**: foreground (a terminal they can `Ctrl-C`) or systemd / launchd service. Default to **foreground first**, only convert to a service after pairing succeeds.

If the user has not yet decided on a backend, run §5 (from-scratch) instead of §3.

## 1. Probe the environment

Run these to characterize the host. Do not edit anything yet.

```sh
uname -sm                                   # Linux/Darwin, x86_64/arm64
command -v usepod-agent && usepod-agent version  # already installed?
command -v systemctl >/dev/null && echo "systemd available"
command -v brew >/dev/null && echo "Homebrew available"
# Backend probes (curl any that respond with HTTP 200):
curl -fsS http://localhost:8000/v1/models  2>/dev/null | head -c 200  # vLLM
curl -fsS http://localhost:8080/v1/models  2>/dev/null | head -c 200  # llama.cpp
curl -fsS http://localhost:1234/v1/models  2>/dev/null | head -c 200  # LM Studio
curl -fsS http://localhost:11434/api/tags  2>/dev/null | head -c 200  # Ollama
```

Report what you found in one paragraph (platform, agent presence, detected backends). Then proceed.

## 2. Decision tree

- **Backend running, agent missing** → §3 (install) → §4 (pair) → optional §6 (service)
- **Backend running, agent installed** → §4 (pair) → optional §6 (service)
- **No backend running** → §5 (from-scratch) → §4 (pair) → optional §6 (service)
- **Already paired** (a `~/.usepod-agent/agent.toml` with a non-empty `[coordinator] enrollment_code` exists) → skip to §7 (verify)

## 3. Install the agent

Use the official installer. Do not build from source unless the user explicitly asks — released binaries are sha256-verified against the GitHub release.

```sh
curl -fsSL https://usepod.ai/install.sh | sh
```

Prefix override if `/usr/local/bin` is not writable and they don't want sudo:

```sh
USEPOD_PREFIX="$HOME/.local" curl -fsSL https://usepod.ai/install.sh | sh
# then ensure $HOME/.local/bin is on PATH
```

Verify:

```sh
usepod-agent version
```

If the user already has `usepod-agent` and wants the latest:

```sh
usepod-agent upgrade        # added in v0.2.1+
# or, on older agents:
curl -fsSL https://usepod.ai/install.sh | sh
```

## 4. Pair the agent (device-flow)

This is the standard onboarding. The agent issues a short pair code, the operator types it into `https://usepod.ai/host/pair`, the dashboard claims it, the agent writes a config and exits. The flow runs against a coordinator preview; nothing is committed until the operator clicks Claim.

```sh
usepod-agent setup
```

What happens during `setup`:

1. The agent probes localhost backends (same probes as §1) and reports them as **capabilities**.
2. It generates an Ed25519 identity, requests a pair code from `api.usepod.ai`, and prints the code (e.g. `ABCD-EFGH`).
3. Long-polls until the operator submits the code on `/host/pair`, picks which models to expose and at what price, and clicks Claim.
4. Writes `~/.usepod-agent/agent.toml` with the coordinator URL, identity path, and the resolved provider id.

While `setup` is running, **tell the user**:

- The pair code (read it from setup's stdout).
- "Open https://usepod.ai/host/pair, paste this code, pick the models you want to expose, and click Claim."

If `setup` reports no backends detected, stop and go to §5.

If the user wants to override the coordinator (testing/staging):

```sh
usepod-agent setup --coordinator https://staging.api.usepod.ai
```

## 5. From-scratch (no backend yet)

Operator has a GPU and curiosity but no inference software. Use the official bootstrap script — installs llama.cpp + Llama-3.2-3B-Instruct-Q4_K_M.gguf (~2 GB) and the agent.

```sh
bash <(curl -fsSL https://usepod.ai/start-from-scratch.sh)
```

Then start `llama-server` in a separate, persistent shell:

```sh
llama-server \
  -m "$HOME/.usepod-agent/models/Llama-3.2-3B-Instruct-Q4_K_M.gguf" \
  --host 0.0.0.0 --port 8080
```

Once that prints `server is listening on ... 8080`, return to §4.

## 6. Run as a service (recommended after first successful pair)

**Only do this after `usepod-agent run` has stayed connected for ≥2 minutes in foreground.** Premature service-ification hides config errors behind systemd's restart loop and burns operator time.

### Linux (systemd, system-level)

```ini
# /etc/systemd/system/usepod-agent.service
[Unit]
Description=Use Pod provider agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=%i
ExecStart=/usr/local/bin/usepod-agent run
Restart=on-failure
RestartSec=5
# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/home/%i/.usepod-agent

[Install]
WantedBy=multi-user.target
```

Activate (replace `<user>` with the operator's username):

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now usepod-agent@<user>.service
sudo systemctl status usepod-agent@<user>.service
journalctl -u usepod-agent@<user>.service -f
```

### Linux (systemd, user-level — no sudo)

```sh
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/usepod-agent.service <<'EOF'
[Unit]
Description=Use Pod provider agent
After=network-online.target

[Service]
Type=simple
ExecStart=%h/.local/bin/usepod-agent run
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable --now usepod-agent.service
loginctl enable-linger "$USER"   # keep the unit running across logouts
```

### macOS (launchd)

```xml
<!-- ~/Library/LaunchAgents/ai.usepod.agent.plist -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>            <string>ai.usepod.agent</string>
  <key>ProgramArguments</key> <array>
    <string>/usr/local/bin/usepod-agent</string>
    <string>run</string>
  </array>
  <key>RunAtLoad</key>        <true/>
  <key>KeepAlive</key>        <true/>
  <key>StandardOutPath</key>  <string>/tmp/usepod-agent.out.log</string>
  <key>StandardErrorPath</key><string>/tmp/usepod-agent.err.log</string>
</dict>
</plist>
```

```sh
launchctl load -w ~/Library/LaunchAgents/ai.usepod.agent.plist
launchctl list | grep ai.usepod.agent
```

## 7. Verify the agent is online

```sh
usepod-agent enroll                  # prints public_key + provider_id
journalctl -u usepod-agent@$USER -n 50 --no-pager  # systemd hosts
# or, for foreground / launchd:
tail -n 50 /tmp/usepod-agent.err.log 2>/dev/null
```

Look for `connected to coordinator` and a heartbeat line every ~15 s. Then visit `https://usepod.ai/host/dashboard` — the provider should show **online** with the registered models.

## 8. Common failures

| Symptom | Likely cause | Fix |
|---|---|---|
| `setup` exits "no backends detected" | nothing listening on 8000/8080/1234/11434 | Start the backend first; rerun §1 probes |
| `setup` long-poll times out | operator never claimed in dashboard within 10 min | Re-run `usepod-agent setup` for a fresh code |
| Agent reconnects every few seconds | `coordinator.url` is `ws://` (insecure) | Use `wss://`, or pass `--allow-insecure` only for local testing |
| `enroll` prints `<not yet enrolled>` | `setup` was never run, or its config was not written | Re-run `usepod-agent setup`; check `~/.usepod-agent/agent.toml` exists |
| Pair code rejected as expired | TTL is 10 minutes | Issue a fresh code with `usepod-agent setup` |
| Earnings stay at 0 with traffic | Model id in dashboard does not match what the backend serves | Check `curl http://localhost:<port>/v1/models` and re-pair with the exact model id |
| systemd restart-loops with `Permission denied` on identity.key | Service is running as a different user than the one who ran setup | Match `User=` in the unit to the user who owns `~/.usepod-agent/` |

## Hard rules — do not skip

- **Never paste the operator's identity key, API keys, or wallet seed into a chat tool.** The Ed25519 identity at `~/.usepod-agent/identity.key` must stay on the host.
- **Do not enable a system-level service before the foreground run has succeeded.** Service-mode obscures config errors.
- **Do not edit `agent.toml` to set an `enrollment_code` manually unless the user explicitly opts out of `setup`.** The pair flow is the supported path; the manual path is a v0.1 fallback.
- **Do not modify `~/.usepod-agent/identity.key` or delete it without warning.** Deleting it forks the on-coordinator identity from the host and forces re-enrollment from scratch.
- **For BYOK operators (Venice / OpenRouter / Together / Groq) there is no agent to install.** Stop and direct them to `https://usepod.ai/host/byok` instead.

## Reporting back

When done, give the user:

- `provider_id` (from `usepod-agent enroll`)
- Service status one-liner (foreground / systemd / launchd)
- Direct link to their dashboard: `https://usepod.ai/host/dashboard`
