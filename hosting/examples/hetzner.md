# Hetzner Cloud

## Create server

```bash
hcloud server create \
  --type cx21 \
  --image ubuntu-24.04 \
  --name codery \
  --ssh-key your-key-name \
  --user-data-from-file hosting/cloud-init.yaml
```

Or via [Hetzner Console](https://console.hetzner.cloud): create server, paste
`cloud-init.yaml` contents into **User data** field.

## Post-provision

1. Note the server's public IP
2. Add GitHub Actions secrets:
   - `DEPLOY_HOST` = server IP (or Tailscale IP after `proxy/host-setup.sh`)
   - `DEPLOY_SSH_KEY` = private key matching the public key in `cloud-init.yaml`
3. Run the **Setup Host** workflow to install Caddy, Tailscale, supervisord
4. Push to `main` to trigger container deploys

## Notes

- cx21 (2 vCPU, 4 GB) is the minimum recommended size
- ARM instances (CAX series) are not supported (x86_64 only)
