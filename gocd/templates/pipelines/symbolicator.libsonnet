local getsentry = import 'github.com/getsentry/gocd-jsonnet/libs/getsentry.libsonnet';
local gocdtasks = import 'github.com/getsentry/gocd-jsonnet/libs/gocd-tasks.libsonnet';

// Only the US region has a canary deployment.
local deploy_canary_stage(region) =
  if region != 'us' then
    []
  else
    [
      {
        'deploy-canary': {
          fetch_materials: true,
          jobs: {
            create_sentry_release: {
              environment_variables: {
                SENTRY_URL: 'https://sentry.my.sentry.io/',
                ENVIRONMENT: 'canary',
                // Temporary; self-service encrypted secrets aren't implemented yet.
                // This should really be rotated to an internal integration token.
                SENTRY_AUTH_TOKEN: '{{SECRET:[devinfra-temp][symbolicator_sentry_auth_token]}}',
              },
              timeout: 1200,
              elastic_profile_id: 'symbolicator',
              tasks: [
                gocdtasks.script(importstr '../bash/create-sentry-release.sh'),
              ],
            },
            deploy: {
              timeout: 1200,
              elastic_profile_id: 'symbolicator',
              environment_variables: {
                LABEL_SELECTOR: 'service=symbolicator,deploy_if_canary=true',
              },
              tasks: [
                gocdtasks.script(importstr '../bash/deploy.sh'),
              ],
            },
          },
        },
      },
    ];

function(region) {
  environment_variables: {
    SENTRY_REGION: region,
  },
  lock_behavior: 'unlockWhenFinished',
  materials: {
    symbolicator_repo: {
      git: 'git@github.com:getsentry/symbolicator.git',
      shallow_clone: true,
      branch: 'master',
      destination: 'symbolicator',
    },
  },
  stages: [
    {
      checks: {
        fetch_materials: true,
        environment_variables: {
          // Required for checkruns.
          GITHUB_TOKEN: '{{SECRET:[devinfra-github][token]}}',
        },
        jobs: {
          checks: {
            timeout: 1200,
            elastic_profile_id: 'symbolicator',
            tasks: [
              gocdtasks.script(importstr '../bash/github-checks.sh'),
            ],
          },
        },
      },
    },
  ] + deploy_canary_stage(region) + [
    {
      'deploy-primary': {
        [if getsentry.is_st(region) then null else 'approval']: {
          type: 'manual',
        },
        fetch_materials: true,
        jobs: {
          create_sentry_release: {
            environment_variables: {
              SENTRY_URL: 'https://sentry.my.sentry.io/',
              ENVIRONMENT: 'production',
              // Temporary; self-service encrypted secrets aren't implemented yet.
              // This should really be rotated to an internal integration token.
              SENTRY_AUTH_TOKEN: '{{SECRET:[devinfra-temp][symbolicator_sentry_auth_token]}}',
            },
            timeout: 1200,
            elastic_profile_id: 'symbolicator',
            tasks: [
              gocdtasks.script(importstr '../bash/create-sentry-release.sh'),
            ],
          },
          deploy: {
            timeout: 1200,
            elastic_profile_id: 'symbolicator',
            environment_variables: {
              LABEL_SELECTOR: 'service=symbolicator',
            },
            tasks: [
              gocdtasks.script(importstr '../bash/deploy.sh'),
            ],
          },
        },
      },
    },
  ],
}
