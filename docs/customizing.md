# Customizing Codery

This guide covers personalizations that aren't part of the default setup.

## GitHub App for Container Authentication

The sandbox and apps containers can authenticate with GitHub using a GitHub App instead of a personal access token. This is optional — personal tokens work too.

### Setup

1. Create a GitHub App at **Settings → Developer settings → GitHub Apps**
2. Set permissions: Contents (read/write), Packages (read), Issues/PRs (read/write)
3. Generate a private key (.pem file)
4. Install the app on your repositories
5. Add the PEM file to your VPS:

```bash
# On the VPS host
sudo mkdir -p /opt/codery
sudo cp your-key.pem /opt/codery/github-app.pem
sudo chmod 600 /opt/codery/github-app.pem
```

6. Add to your `ENV_FILE` secret:

```
GITHUB_APP_ID=12345
GITHUB_APP_INSTALLATION_ID=67890
GITHUB_APP_SLUG=your-app-name
```

The container entrypoint scripts (`20-github-auth.sh`) automatically detect these env vars and authenticate `gh` CLI using the GitHub App.

## Custom Domain

Add to your `ENV_FILE`:

```
DOMAIN_NAME=yourdomain.com
```

Ensure DNS for your subdomains points to your VPS IP. Caddy handles TLS automatically.

## API Keys

Add any API keys your setup needs to the `ENV_FILE` secret. Common ones:

```
ANTHROPIC_API_KEY=sk-ant-...
OPENAI_API_KEY=sk-...
OPENROUTER_API_KEY=sk-or-...
```

The sandbox container passes all env vars through to OpenCode and any tools running inside it.

## Adding Services to the Apps Container

1. Write a supervisor config:

```ini
; containers/apps/supervisor/conf.d/myapp.conf
[program:myapp]
command=bun run /home/gem/projects/myapp/server.ts
directory=/home/gem/projects/myapp
user=root
autostart=true
autorestart=true
stdout_logfile=/var/log/supervisor/myapp.log
stdout_logfile_maxbytes=10MB
```

2. Add a route in `proxy/apps-routes.json`:

```json
{"subdomain": "myapp.yourdomain.com", "port": 8080}
```

3. Push to `main` or trigger the Build Apps workflow.

## SSH into the Sandbox

The sandbox exposes SSH on port 2222. Add your public key:

```bash
# On the VPS host
echo "ssh-ed25519 AAAA..." | sudo tee -a /opt/codery/ssh/authorized_keys
```

Then connect:

```bash
ssh -p 2222 gem@<vps-ip>
```
