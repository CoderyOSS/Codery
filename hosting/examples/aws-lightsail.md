# AWS Lightsail

## Create instance

```bash
aws lightsail create-instances \
  --instance-name codery \
  --availability-zone us-east-1a \
  --blueprint-id ubuntu_24_04 \
  --bundle-id medium_2_0 \
  --user-data file://hosting/cloud-init.yaml
```

Or via [AWS Console](https://lightsail.aws.amazon.com): create instance, select
Ubuntu 24.04, paste `cloud-init.yaml` into **Launch script**.

## Post-provision

1. Note the instance's public IP (or attach a static IP)
2. Open ports in Lightsail firewall: 22 (SSH), 80 (HTTP), 443 (HTTPS)
3. Add GitHub Actions secrets:
   - `DEPLOY_HOST` = instance IP (or Tailscale IP after `proxy/host-setup.sh`)
   - `DEPLOY_SSH_KEY` = private key matching the public key in `cloud-init.yaml`
4. Run the **Setup Host** workflow to install Caddy, Tailscale, supervisord
5. Push to `main` to trigger container deploys

## Notes

- medium_2_0 (2 vCPU, 4 GB) is the minimum recommended size
- Lightsail has its own firewall — configure it separately from UFW in cloud-init
- For EC2: use the same `cloud-init.yaml` as user-data; t3.medium is the equivalent size
