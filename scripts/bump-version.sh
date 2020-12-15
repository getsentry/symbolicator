#!/bin/bash
set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
    echo "Symbolicator can only be released on Linux!"
    echo "Please use the GitHub Action instead."
    exit 1
fi

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
cd $SCRIPT_DIR/..

OLD_VERSION="${1}"
NEW_VERSION="${2}"

echo "Current version: ${OLD_VERSION}"
echo "Bumping version: ${NEW_VERSION}"

TOML_FILES="$(git ls-files '*Cargo.toml')"
perl -pi -e "s/^version = .*\$/version = \"$NEW_VERSION\"/" $TOML_FILES

cargo update -p symbolicator
