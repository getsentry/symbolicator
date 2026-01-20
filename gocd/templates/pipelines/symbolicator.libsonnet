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
                SENTRY_URL: 'https://sentry.io',
                // We use the Relay token, it is named in S4S2 to indicate usage for Relay and Symbolicator.
                SENTRY_AUTH_TOKEN: '{{SECRET:[devinfra-temp][relay_sentry_s4s2_auth_token]}}',
                SENTRY_ORG: 'sentry-s4s2',
                SENTRY_PROJECT: 'symbolicator',
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
              SENTRY_URL: 'https://sentry.io',
              // We use the Relay token, it is named in S4S2 to indicate usage for Relay and Symbolicator.
              SENTRY_AUTH_TOKEN: '{{SECRET:[devinfra-temp][relay_sentry_s4s2_auth_token]}}',
              SENTRY_ORG: 'sentry-s4s2',
              SENTRY_PROJECT: 'symbolicator',
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
