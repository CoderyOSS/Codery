#!/usr/bin/env python3
"""Generate Launchy service configs from .devcontainer/devcontainer.json.
Writes one JSON file per service to the output directory.
Includes infrastructure services (ssh-agent, sshd, nginx) plus apps from devcontainer.json.
Usage: python3 gen-launchy-conf.py [output_dir]
Run from repo root.
"""
import json, os, sys

with open('.devcontainer/devcontainer.json') as f:
    dc = json.load(f)

out_dir = sys.argv[1] if len(sys.argv) > 1 else 'containers/apps/launchy/built-in'
os.makedirs(out_dir, exist_ok=True)

for name in os.listdir(out_dir):
    if name.endswith('.json'):
        os.remove(os.path.join(out_dir, name))

infra = [
    {
        "name": "ssh-agent",
        "command": ["bash", "-c", "rm -f /tmp/ssh-agent.sock && exec ssh-agent -a /tmp/ssh-agent.sock -d"],
        "restart": "always",
        "priority": 10,
    },
    {
        "name": "ssh-agent-keys",
        "command": ["/usr/local/bin/ssh-agent-add-keys.sh"],
        "restart": "never",
        "priority": 20,
    },
    {
        "name": "sshd",
        "command": ["/usr/sbin/sshd", "-D", "-e"],
        "restart": "always",
        "priority": 30,
    },
    {
        "name": "nginx",
        "command": ["nginx", "-g", "daemon off;"],
        "restart": "always",
        "priority": 30,
    },
]

apps = dc['customizations']['codery'].get('apps', [])

for svc in infra:
    path = os.path.join(out_dir, f"{svc['name']}.json")
    with open(path, 'w') as f:
        json.dump(svc, f, indent=2)
        f.write('\n')
    print(f"[gen-launchy] Wrote infra config for {svc['name']}")

for app in apps:
    svc = {
        "name": app['name'],
        "command": app['command'].split(),
        "directory": app['directory'],
        "user": "gem",
        "restart": "always",
        "priority": 100,
    }
    if app.get('env'):
        svc['env'] = app['env']
    path = os.path.join(out_dir, f"{app['name']}.json")
    with open(path, 'w') as f:
        json.dump(svc, f, indent=2)
        f.write('\n')
    print(f"[gen-launchy] Wrote app config for {app['name']}")

print(f"[gen-launchy] Total: {len(infra)} infra + {len(apps)} app configs in {out_dir}")
