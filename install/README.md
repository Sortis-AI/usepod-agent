# Use Pod provider-agent — install

Run a GPU? Earn USDC by serving inference. The provider agent connects your
local backend (vLLM / llama.cpp / LM Studio / Ollama) or BYOK keys (OpenRouter
/ Venice) to the Use Pod marketplace coordinator.

This directory ships the installer scripts, a systemd unit template, and a
container image you can use instead.

## One-line install

Linux / macOS:

```sh
curl -fsSL https://usepod.ai/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://usepod.ai/install.ps1 | iex
```

The script:

1. Detects platform via `uname -s` / `uname -m` and picks the right asset
   (`linux-x64`, `linux-arm64`, `darwin-arm64`, or `windows-x64`).
2. Reads the latest version pointer from `https://usepod.ai/agent-latest`
   (falling back to a hardcoded version if that endpoint is unreachable).
3. Downloads the matching binary plus its `.sha256` checksum from
   `github.com/usepod-ai/provider-agent/releases`.
4. Verifies the checksum (`sha256sum -c` on Linux, `shasum -a 256 -c` on
   macOS, `Get-FileHash` on Windows).
5. Installs to `/usr/local/bin/usepod-agent` (Linux/macOS) or
   `%ProgramFiles%\usepod\usepod-agent.exe` (Windows, with the install dir
   added to the user `PATH`).

Supported platforms: `Linux-x86_64`, `Linux-aarch64`, `Darwin-arm64`,
`Windows-x64`. Intel Macs, FreeBSD, and 32-bit targets are not supported.

### Environment overrides

| Variable           | Default                          | Purpose                              |
| ------------------ | -------------------------------- | ------------------------------------ |
| `USEPOD_VERSION`   | (latest from `agent-latest`)     | Pin a specific release tag (`v0.2.1`). |
| `USEPOD_PREFIX`    | `/usr/local`                     | Install prefix on Linux/macOS.       |
| `USEPOD_BASE_URL`  | `https://usepod.ai`              | Base URL for the version pointer.    |
| `USEPOD_REPO`      | `usepod-ai/provider-agent`       | GitHub releases repo to download from. |

## Manual install

If you'd rather not pipe `curl` into a shell:

```sh
# 1. Pick your asset.
VERSION=v0.1.0
ASSET=usepod-agent-linux-x64
BASE=https://github.com/usepod-ai/provider-agent/releases/download/$VERSION

# 2. Download binary + checksum.
curl -fsSLO "$BASE/$ASSET"
curl -fsSLO "$BASE/$ASSET.sha256"

# 3. Verify.
sha256sum -c "$ASSET.sha256"

# 4. Install.
sudo install -m 755 "$ASSET" /usr/local/bin/usepod-agent

# 5. Confirm.
usepod-agent version
```

## systemd setup (Linux servers)

1. Create the service user and state directory:

   ```sh
   sudo useradd --system --home /var/lib/usepod-agent --shell /usr/sbin/nologin usepod
   sudo install -d -o usepod -g usepod -m 0750 /var/lib/usepod-agent
   ```

2. Drop the agent config in place. The agent will create its identity key
   under `/var/lib/usepod-agent/identity.pem` on first run unless `agent.toml`
   points elsewhere.

   ```sh
   sudo install -o usepod -g usepod -m 0640 agent.toml /etc/usepod-agent/agent.toml
   ```

3. Install the systemd unit shipped with this repo:

   ```sh
   sudo cp usepod-agent.service /etc/systemd/system/usepod-agent.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now usepod-agent
   ```

4. Watch logs:

   ```sh
   journalctl -u usepod-agent -f
   ```

The shipped unit hardens the service with `ProtectSystem=strict`,
`PrivateTmp=true`, `NoNewPrivileges=true`, and friends. If your backend
requires extra paths, add them to `ReadWritePaths=` rather than relaxing the
overall sandbox.

## Docker

Image: `usepod/provider-agent:<version>` (also `:latest`).

```sh
docker volume create usepod-agent
docker run -d --name usepod-agent \
    --restart=always \
    -v usepod-agent:/var/lib/usepod-agent \
    -v $PWD/agent.toml:/etc/usepod-agent/agent.toml:ro \
    -p 9090:9090 \
    usepod/provider-agent:latest \
    --config /etc/usepod-agent/agent.toml
```

The volume holds the identity keypair so it survives container replacement.
Port `9090` exposes the local Prometheus metrics endpoint; expose it only on
trusted networks.

## Enrollment walkthrough

```sh
usepod-agent enroll                     # prints public key + enrollment code
# Paste the enrollment code at https://usepod.ai/host
# Send the bond (USDC) to the displayed deposit address
usepod-agent run                        # starts serving jobs
```

`usepod-agent validate` parses the config without touching the network — useful
to confirm a deployment before enabling the systemd unit.

## Troubleshooting

- **`unsupported platform` from `install.sh`** — Intel Mac, FreeBSD, or a
  32-bit OS. Either run inside a Linux container or build from source.
- **`failed to download ...`** — usually a corporate proxy or firewall.
  `curl https://github.com` should succeed before re-running the installer.
  Set `https_proxy` / `HTTPS_PROXY` if you go through a proxy.
- **`checksum verification failed`** — the download was truncated or
  corrupted. Re-run the installer; if it persists, the release is bad — open
  an issue.
- **systemd `status=200/CHDIR`** — the `usepod` user can't `cd` into
  `/var/lib/usepod-agent`. Check ownership and the `ReadWritePaths=` list.
- **Identity key location** — defaults to `~/.local/share/usepod-agent/`
  for an interactive run; the systemd unit pins it to `/var/lib/usepod-agent`.
  See `agent.toml`'s `[identity]` section to override.
- **Logs** — `journalctl -u usepod-agent -f` (systemd),
  `docker logs -f usepod-agent` (Docker), stdout otherwise. Bump verbosity
  with `--log-level debug`.
- **Prometheus metrics** — `curl http://127.0.0.1:9090/metrics` from the
  same host once the agent is running.

## Security notes

- The installer verifies SHA-256 against the per-asset `.sha256` file uploaded
  with the same release. Code-signing for Windows / macOS and Sigstore
  signatures are deferred to a later release (see `plan/V2_AGENT_SPEC.md`
  §10.3).
- Do not commit your identity keypair or `agent.toml`. Treat the identity
  key as the credential that authenticates you to the marketplace.
- For untrusted shared hosts, prefer the Docker image so the agent runs in a
  pid/network/filesystem-isolated container.
