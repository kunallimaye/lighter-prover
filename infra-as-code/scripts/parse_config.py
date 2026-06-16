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
    defaults = vms.get('default', {}) if isinstance(vms, dict) else {}
    if not isinstance(defaults, dict):
      defaults = {}

    def_machine = str(defaults.get('machine_type', 'c4-standard-8'))
    def_zone = str(defaults.get('zone', 'us-central1-a'))
    def_disk_size = int(defaults.get('disk_size_gb', 100))
    def_disk_type = str(defaults.get('disk_type', 'pd-ssd'))
    def_sa = str(defaults.get('service_account', defaults.get('runtime_sa', '')))

    cleaned = {}
    if isinstance(vms, dict):
      for k, v in vms.items():
        if k == 'default' or not isinstance(v, dict):
          continue
        cleaned[k] = {
            'machine_type': str(v.get('machine_type', def_machine)),
            'zone': str(v.get('zone', def_zone)),
            'disk_size_gb': int(v.get('disk_size_gb', def_disk_size)),
            'disk_type': str(v.get('disk_type', def_disk_type)),
            'service_account': str(v.get('service_account', v.get('runtime_sa', def_sa))),
        }
    print(json.dumps(cleaned))

  elif out_mode == 'target':
    target = data.get('gcp', {}).get('target', {})
    if not target and 'target' in data:
      target = data.get('target', {})

    target_sas = {}
    build_email = ""
    runtime_email = ""

    build_machine = "UNSPECIFIED"
    if isinstance(target, dict):
      build_machine = str(target.get('build_machine_type', 'UNSPECIFIED'))
      for sa_key, sa_obj in target.items():
        if sa_key == 'build_machine_type' or (isinstance(sa_obj, str) and '@' not in sa_obj):
          continue
        email = sa_obj.get('email', '') if isinstance(sa_obj, dict) else str(sa_obj)
        roles = sa_obj.get('roles', []) if isinstance(sa_obj, dict) else []
        target_sas[sa_key] = {'email': email, 'roles': roles}

        if sa_key == 'build_sa' or 'build' in sa_key:
          build_email = email
        elif sa_key == 'runtime_sa' or 'runtime' in sa_key:
          runtime_email = email

    # Fallback resolution if standard keys weren't explicit
    if not build_email and target_sas:
      build_email = list(target_sas.values())[0]['email']
    if not runtime_email and len(target_sas) > 1:
      runtime_email = list(target_sas.values())[1]['email']
    elif not runtime_email and target_sas:
      runtime_email = build_email

    cleaned = {
        'target_sas': target_sas,
        'builder_sa_email': build_email,
        'runtime_sa_email': runtime_email,
        'build_machine_type': build_machine,
    }
    print(json.dumps(cleaned))


if __name__ == '__main__':
  main()
