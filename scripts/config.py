#!/usr/bin/env python3
"""Parse config.toml (role-axis + environment-axis) and emit shell exports.

Resolution order (#141): env > role > defaults > error.

Schema (#141):

    [gcp.defaults]          # required catch-all (the 90% case)
    project = "..."
    region  = "..."

    [gcp.orchestration]     # role override, empty => inherit defaults
    project = ""
    region  = ""

    [gcp.build]             # role override
    project = ""
    region  = ""

    [gcp.runtime]           # role override
    project = ""
    region  = ""

Environment-axis layering (#115) is layered ON TOP of the role axis:

    [gcp.production.runtime]
    project = "acme-prod"   # only overrides runtime in production

The active environment is selected via ENVIRONMENT env var (default: staging).

The .env file is read by scripts/common.sh (NOT here); .env values
override everything below at the shell level via export precedence.

Same parser is the source of truth for shell scripts and Terraform.
Terraform sees the resolved values as TF_VAR_* env vars set in the
Cloud Build apply config.
"""

import os
import sys

try:
    import tomllib
except ModuleNotFoundError:
    try:
        import tomli as tomllib  # Python < 3.11 fallback
    except ModuleNotFoundError:
        print(
            "ERROR: config.py requires Python 3.11+ (for stdlib tomllib) "
            "or the 'tomli' package on older Python.\n"
            "Fix: upgrade Python to 3.11+ OR run: pip install tomli",
            file=sys.stderr,
        )
        sys.exit(1)


def _resolve(config: dict, env: str, role: str, key: str, default=None):
    """Resolve a key with precedence: env-role > role > env > defaults > default.

    Lookups (first hit wins):
      [gcp.<env>.<role>].<key>
      [gcp.<role>].<key>
      [gcp.<env>].<key>
      [gcp.defaults].<key>
    """
    gcp = config.get('gcp', {})
    candidates = [
        gcp.get(env, {}).get(role, {}).get(key),
        gcp.get(role, {}).get(key),
        gcp.get(env, {}).get(key),
        gcp.get('defaults', {}).get(key),
    ]
    for v in candidates:
        if v not in (None, ''):
            return v
    return default


def _emit(name: str, value):
    """Shell-quote a value and emit `KEY='value'` for eval."""
    if value is None:
        value = ''
    s = str(value).replace("'", "'\\''")
    print(f"{name}='{s}'")


def main():
    config_path = os.path.join(os.path.dirname(__file__), '..', 'config.toml')
    if not os.path.exists(config_path):
        # No config.toml? Quiet exit — scripts/common.sh handles defaults.
        return

    with open(config_path, 'rb') as f:
        config = tomllib.load(f)

    env = os.environ.get('ENVIRONMENT', 'staging')

    # ─── Project-level ─────────────────────────────────────────
    project = config.get('project', {})
    project_name = project.get('name', 'app')

    _emit('ENVIRONMENT', env)
    _emit('PROJECT_NAME', project_name)

    # ─── Three-role topology ──────────────────────────────────
    # Resolve each role's project + region with env > role > defaults precedence.
    # Defaults must be set; otherwise resolution returns '' and the bash
    # layer dies with a clear error.
    orch_project    = _resolve(config, env, 'orchestration', 'project', '')
    orch_region     = _resolve(config, env, 'orchestration', 'region',  '')
    build_project   = _resolve(config, env, 'build',         'project', '')
    build_region    = _resolve(config, env, 'build',         'region',  '')
    runtime_project = _resolve(config, env, 'runtime',       'project', '')
    runtime_region  = _resolve(config, env, 'runtime',       'region',  '')

    if not _resolve(config, env, 'defaults', 'project'):
        # The role axis tolerates empty roles ONLY when defaults is set.
        # Surface this loudly: a single typo in [gcp.defaults] would
        # silently produce three empty role projects.
        print(
            "echo 'ERROR: [gcp.defaults].project is required in config.toml "
            "(role-axis topology requires a catch-all default)' >&2",
            file=sys.stdout,
        )
        print("exit 1", file=sys.stdout)
        sys.exit(1)

    _emit('ORCH_PROJECT',    orch_project)
    _emit('ORCH_REGION',     orch_region)
    _emit('BUILD_PROJECT',   build_project)
    _emit('BUILD_REGION',    build_region)
    _emit('RUNTIME_PROJECT', runtime_project)
    _emit('RUNTIME_REGION',  runtime_region)

    # Legacy aliases for back-compat with snippets that still use the
    # pre-role-topology names. Map to the most likely role.
    _emit('GCP_PROJECT', runtime_project)
    _emit('GCP_REGION',  runtime_region or 'us-central1')
    _emit('CB_PROJECT',  build_project)

    # ─── Resource & deployment knobs (env > defaults) ──────────
    _emit('DOMAIN',           _resolve(config, env, 'runtime', 'domain',           ''))
    _emit('DNS_PROJECT_ID',   _resolve(config, env, 'runtime', 'dns_project_id',   ''))
    _emit('DNS_MANAGED_ZONE', _resolve(config, env, 'runtime', 'dns_managed_zone', ''))
    _emit('DNS_RECORD_NAME',  _resolve(config, env, 'runtime', 'dns_record_name',  ''))
    _emit('MIN_INSTANCES',    _resolve(config, env, 'runtime', 'min_instances', '0'))
    _emit('MAX_INSTANCES',    _resolve(config, env, 'runtime', 'max_instances', '3'))
    _emit('CPU',              _resolve(config, env, 'runtime', 'cpu',    '1'))
    _emit('MEMORY',           _resolve(config, env, 'runtime', 'memory', '512Mi'))
    _emit('INGRESS',          _resolve(config, env, 'runtime', 'ingress', 'all'))

    # ─── Service accounts ──────────────────────────────────────
    # Agent SA in orchestration project, builder SA in build project,
    # runtime SA in runtime project. Names default to <project-name>-<role>.
    agent_sa_name   = _resolve(config, env, 'orchestration', 'agent_sa',   f'{project_name}-agent')
    builder_sa_name = _resolve(config, env, 'build',         'builder_sa', f'{project_name}-builder')
    runtime_sa_name = _resolve(config, env, 'runtime',       'runtime_sa', f'{project_name}-runtime')

    _emit('AGENT_SA_NAME',   agent_sa_name)
    _emit('BUILDER_SA_NAME', builder_sa_name)
    _emit('RUNTIME_SA_NAME', runtime_sa_name)

    _emit('AGENT_SA_EMAIL',   f'{agent_sa_name}@{orch_project}.iam.gserviceaccount.com')
    _emit('BUILDER_SA_EMAIL', f'{builder_sa_name}@{build_project}.iam.gserviceaccount.com')
    _emit('RUNTIME_SA_EMAIL', f'{runtime_sa_name}@{runtime_project}.iam.gserviceaccount.com')

    # Legacy: CB_SERVICE_ACCOUNT used to mean "builder SA email".
    _emit('CB_SERVICE_ACCOUNT', f'{builder_sa_name}@{build_project}.iam.gserviceaccount.com')

    # ─── Custom role ID ────────────────────────────────────────
    # GCP custom role IDs must be camelCase (no dashes/underscores).
    def _camel(name: str) -> str:
        parts = name.replace('_', '-').split('-')
        return parts[0] + ''.join(p[:1].upper() + p[1:] for p in parts[1:])

    deployer_role_id = _resolve(
        config, env, 'orchestration', 'deployer_role_id',
        f'{_camel(project_name)}Deployer',
    )
    _emit('DEPLOYER_ROLE_ID', deployer_role_id)

    # ─── AR repo + TF state ────────────────────────────────────
    _emit('AR_REPO',         _resolve(config, env, 'build', 'ar_repo', project_name))
    _emit('TF_STATE_BUCKET', _resolve(config, env, 'build', 'state_bucket', ''))
    # TF state prefix is auto-derived; rarely overridden.
    _emit('TF_STATE_PREFIX', f'{project_name}/{env}')

    # ─── Agent role expiry (days) ─────────────────────────────
    _emit('AGENT_ROLE_EXPIRY_DAYS', _resolve(config, env, 'orchestration', 'agent_role_expiry_days', '30'))


if __name__ == '__main__':
    main()
