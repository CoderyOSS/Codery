# DigitalOcean

## Create droplet

```bash
doctl compute droplet create codery \
  --image ubuntu-24-04-x64 \
  --size s-2vcpu-4gb \
  --region nyc1 \
  --ssh-keys your-key-fingerprint \
  --user-data-file hosting/cloud-init.yaml \
  --enable-monitoring
```

Or via [DigitalOcean Console](https://cloud.digitalocean.com): create droplet,
select **User data** under "Select additional options", paste `cloud-init.yaml`.

## Post-provision

1. Note the droplet's public IP
2. Add GitHub Actions secrets:
   - `DEPLOY_HOST` = droplet IP (or Tailscale IP after `proxy/host-setup.sh`)
   - `DEPLOY_SSH_KEY` = private key matching the public key in `cloud-init.yaml`
3. Run the **Setup Host** workflow to install Caddy, Tailscale, supervisord
4. Push to `main` to trigger container deploys

## Notes

- s-2vcpu-4gb is the minimum recommended size
- DigitalOcean metadata service provides the droplet IP — no manual config needed
