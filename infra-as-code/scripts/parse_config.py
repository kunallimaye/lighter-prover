#!/usr/bin/env python3
import json
import os
import sys

try:
  import tomllib  # Python 3.11+
except ImportError:
  try:
    import tomli as tomllib
  except ImportError:
    import toml as tomllib


def main():
  config_path = sys.argv[1] if len(sys.argv) > 1 else 'config.toml'
  out_mode = sys.argv[2] if len(sys.argv) > 2 else 'vms'

  if not os.path.exists(config_path):
    print('{}')
    return

  try:
    with open(config_path, 'rb') as f:
      data = tomllib.load(f)
  except (TypeError, AttributeError):
    with open(config_path, 'r', encoding='utf-8') as f:
      data = tomllib.load(f)

  if out_mode == 'vms':
    vms = data.get('vms', {})
    cleaned = {}
    if isinstance(vms, dict):
      for k, v in vms.items():
        if isinstance(v, dict):
          cleaned[k] = {
              'machine_type': str(v.get('machine_type', 'c4-standard-8')),
              'zone': str(v.get('zone', 'us-central1-a')),
              'disk_size_gb': int(v.get('disk_size_gb', 100)),
              'disk_type': str(v.get('disk_type', 'pd-ssd')),
          }
    print(json.dumps(cleaned))

  elif out_mode == 'target':
    target = data.get('gcp', {}).get('target', {})
    if not target and 'target' in data:
      target = data.get('target', {})

    build_sa_obj = target.get('build_sa', {})
    runtime_sa_obj = target.get('runtime_sa', {})

    build_email = (
        build_sa_obj.get('email', '')
        if isinstance(build_sa_obj, dict)
        else str(build_sa_obj)
    )
    runtime_email = (
        runtime_sa_obj.get('email', '')
        if isinstance(runtime_sa_obj, dict)
        else str(runtime_sa_obj)
    )

    cleaned = {
        'builder_sa_email': build_email,
        'runtime_sa_email': runtime_email,
        'builder_sa_roles': (
            build_sa_obj.get('roles', []) if isinstance(build_sa_obj, dict) else []
        ),
        'runtime_sa_roles': (
            runtime_sa_obj.get('roles', [])
            if isinstance(runtime_sa_obj, dict)
            else []
        ),
    }
    print(json.dumps(cleaned))


if __name__ == '__main__':
  main()
