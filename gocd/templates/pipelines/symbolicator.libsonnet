local gocdtasks = import 'github.com/getsentry/gocd-jsonnet/v1.0.0/gocd-tasks.libsonnet';

function(region) {
  environment_variables: {
    SENTRY_REGION: region,
  },
  group: 'symbolicator',
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
        approval: {
          type: 'manual',
        },
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
              gocdtasks.script(importstr '../bash/cloudbuild-checks.sh'),
            ],
          },
        },
      },
    },
    {
      deploy_canary: {
        approval: {
          type: 'manual',
        },
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
            tasks: [
              gocdtasks.script(importstr '../bash/deploy-canary.sh'),
            ],
          },
        },
      },
    },
    {
      deploy_primary: {
        approval: {
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
            tasks: [
              gocdtasks.script(importstr '../bash/deploy-primary.sh'),
            ],
          },
        },
      },
    },
  ],
}
