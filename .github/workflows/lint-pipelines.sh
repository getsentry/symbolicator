#!/bin/bash

# gocd-cli does not catch all errors, but does catch some simple issues.
# A better solution may be: https://github.com/GaneshSPatil/gocd-mergeable

echo "GoCD YAML Linting"

find "gocd" -name "*.yaml" -type f -print0 | \
  xargs -0 -I'{}' bash -c 'printf  "\n🔎 Linting {}\n\t" && gocd-cli configrepo syntax --yaml --raw "{}"'
