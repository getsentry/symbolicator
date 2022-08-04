# Common commands for debugging 

set dotenv-load

workspace := env_var_or_default('SENTRY_WORKSPACE', '')

default:
  just --list

bootstrap:
   cargo install --git https://github.com/getsentry/edmgutil --branch main edmgutil
   brew install p7zip
   brew install jq

# create an encrypted workspace
create-workspace name:
   edmgutil new --size 2000 --name {{name}}
   echo "SENTRY_WORKSPACE=/Volumes/{{name}}" > {{justfile_directory()}}/.env

# activate an existing workspace
use-workspace name:
   echo "SENTRY_WORKSPACE=/Volumes/{{name}}" > {{justfile_directory()}}/.env

# run symbolicator
run:
   cargo run --profile local -- --config local.yml run

symsort:
   cargo run -p symsorter -- -o symbols {{workspace}}

# process a dump from the workspace
process DUMP:
   cargo run -p process-event -- {{workspace}}/{{DUMP}} | jq -r '.stacktraces[] | select(.is_requesting == true) | .frames[] | [.trust,.instruction_addr,.function] | @tsv'

# clear symbolicator cache
clear-cache:
   rm -r {{justfile_directory()}}/cache/*

# ls files in workspace
ls:
   ls -la {{workspace}}

   
