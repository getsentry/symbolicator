#!/bin/bash

checks-githubactions-checkruns \
  getsentry/symbolicator \
  ${GO_REVISION_SYMBOLICATOR_REPO} \
  'Tests' \
  'Sentry-Symbolicator Tests' \
  'Upload gocd artifacts'
