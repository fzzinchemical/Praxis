# Real MCP servers for the T4.5 check

Installed outside the repo, pinned, and not a project dependency (ADR 0001 keeps product code Rust-only).

```sh
mkdir -p ~/mcp-servers && cd ~/mcp-servers && npm init -y
npm install --save-exact @modelcontextprotocol/server-filesystem@2.3.0 @playwright/mcp@0.0.85
npx playwright install chromium          # downloads to ~/.cache/ms-playwright
python3 -m venv venv && ./venv/bin/pip install mcp-server-git==2026.11.11
```

Run the real-server check (ignored by default, so verify doesn't need these):

```sh
PATH="$HOME/mcp-servers/node_modules/.bin:$HOME/mcp-servers/venv/bin:$PATH" \
  PLAYWRIGHT_BROWSERS_PATH="$HOME/.cache/ms-playwright" \
  cargo test -p praxis-core --test mcp_real -- --ignored --nocapture
```

## Eval with MCP servers (T4.5)

`cargo xtask eval --model <id> --mcp ~/mcp-servers` mounts the install read-only at `/mcp-servers` inside the sandbox and starts the filesystem and git servers there. The browser server is left out: Chromium may not start inside bwrap, and the real-server check above covers it outside the sandbox. Without `--mcp` the boards stay as before.
