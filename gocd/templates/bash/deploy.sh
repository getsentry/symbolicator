#!/bin/bash

eval $(regions-project_env-vars --region="${SENTRY_REGION}")
/devinfra/scripts/k8s/k8stunnel
k8s-deploy \
  --type="statefulset" \
  --label-selector="${LABEL_SELECTOR}" \
  --image="us-central1-docker.pkg.dev/sentryio/symbolicator/image:${GO_REVISION_SYMBOLICATOR_REPO}" \
  --container-name="symbolicator" \
  --container-name="cleanup"
