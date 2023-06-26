#!/bin/bash

/devinfra/scripts/checks/googlecloud/checkcloudbuild.py \
  ${GO_REVISION_SYMBOLICATOR_REPO} \
  sentryio \
  "us.gcr.io/sentryio/symbolicator"
