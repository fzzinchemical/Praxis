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
