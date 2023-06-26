#!/bin/bash

eval $(/devinfra/scripts/regions/project_env_vars.py --region="${SENTRY_REGION}")
/devinfra/scripts/k8s/k8stunnel
/devinfra/scripts/k8s/k8sdeploy.py \
  --type="statefulset" \
  --label-selector="service=symbolicator,deploy_if_production=true" \
  --image="us.gcr.io/sentryio/symbolicator:${GO_REVISION_SYMBOLICATOR_REPO}" \
  --container-name="symbolicator" \
  --container-name="cleanup"
