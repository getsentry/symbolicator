#!/bin/bash

/devinfra/scripts/checks/githubactions/checkruns.py \
  getsentry/symbolicator \
  ${GO_REVISION_SYMBOLICATOR_REPO} \
  'Tests' \
  'Sentry-Symbolicator Tests' \
  'Upload gocd artifacts'
