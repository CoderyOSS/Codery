#!/usr/bin/env python3
"""Generate proxy/apps-routes.json from .devcontainer/devcontainer.json.
Usage: python3 gen-apps-routes.py [output_path]
Run from repo root.
"""
import json, sys

with open('.devcontainer/devcontainer.json') as f:
    dc = json.load(f)

apps = dc['customizations']['codery'].get('apps', [])
routes = [
    {"subdomain": app['subdomain'], "port": 8080, "internal_port": app['internal_port']}
    for app in apps
]

output = sys.argv[1] if len(sys.argv) > 1 else 'proxy/apps-routes.json'
with open(output, 'w') as f:
    json.dump(routes, f, indent=2)
    f.write('\n')

print(f'[gen-apps-routes] Wrote {len(routes)} route(s) to {output}')
