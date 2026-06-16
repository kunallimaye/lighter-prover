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
  if not os.path.exists(config_path):
    print('{}', end='')
    return

  try:
    with open(config_path, 'rb') as f:
      data = tomllib.load(f)
  except (TypeError, AttributeError):
    with open(config_path, 'r', encoding='utf-8') as f:
      data = tomllib.load(f)

  vms = data.get('vms', {})
  if not isinstance(vms, dict):
    print('{}', end='')
    return

  cleaned = {}
  for k, v in vms.items():
    if not isinstance(v, dict):
      continue
    cleaned[k] = {
        'machine_type': str(v.get('machine_type', 'c4-standard-8')),
        'zone': str(v.get('zone', 'us-central1-a')),
        'disk_size_gb': int(v.get('disk_size_gb', 100)),
        'disk_type': str(v.get('disk_type', 'pd-ssd')),
    }

  print(json.dumps(cleaned))


if __name__ == '__main__':
  main()
