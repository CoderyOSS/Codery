#!/usr/bin/env python3
"""Generate supervisord conf files for each app in .devcontainer/devcontainer.json.
Usage: python3 gen-supervisor-conf.py [output_dir]
Run from repo root.
"""
import json, os, sys

with open('.devcontainer/devcontainer.json') as f:
    dc = json.load(f)

apps = dc['customizations']['codery'].get('apps', [])
out_dir = sys.argv[1] if len(sys.argv) > 1 else 'containers/apps/supervisor/projects.d'
os.makedirs(out_dir, exist_ok=True)

for name in os.listdir(out_dir):
    if name.endswith('.conf'):
        os.remove(os.path.join(out_dir, name))

for app in apps:
    name = app['name']
    env_str = ','.join(f'{k}="{v}"' for k, v in app.get('env', {}).items())
    conf = f'[program:{name}]\n'
    conf += f'command={app["command"]}\n'
    conf += f'directory={app["directory"]}\n'
    conf += 'autostart=true\nautorestart=true\n'
    conf += f'stdout_logfile=/var/log/supervisor/{name}.log\n'
    conf += f'stderr_logfile=/var/log/supervisor/{name}.log\n'
    if env_str:
        conf += f'environment={env_str}\n'
    with open(os.path.join(out_dir, f'{name}.conf'), 'w') as f:
        f.write(conf)
    print(f'[gen-supervisor] Wrote conf for {name}')

if not apps:
    print('[gen-supervisor] No apps defined — projects.d is empty')
