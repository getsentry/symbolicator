# Symbolicator Pipelines

Symbolicator pipelines are rendered using jsonnet.

## Dependencies

You'll need the following dependencies to build the pipelines:

```sh
brew install go-jsonnet jsonnet-bundler yq
```

## Jsonnet

You can render the jsonnet pipelines by running:

```sh
make gocd
```

This will clean, fmt, lint and generate the GoCD pipelines to
`./gocd/generated-pipelines`.

The pipelines are using the https://github.com/getsentry/gocd-jsonnet
libraries to generate the pipeline for each region.

## Files

Below is a description of the directories in the `gocd/` directory.

### `gocd/templates/`

These are a set of jsonnet and libsonnet files which are used
to generate the GoCD pipelines. This avoids duplication across
our pipelines as we deploy to multiple regions.

The `gocd/templates/symbolicator.jsonnet` file is the entry point for the
generated pipelines.

`gocd/templates/pipelines/symbolicator.libsonnet` define the pipeline
behaviors. This library defines the GoCD pipeline, following the same naming
as the [GoCD yaml pipelines](https://github.com/tomzo/gocd-yaml-config-plugin#readme).

`gocd/templates/bash/*.sh` are shell scripts that are inlined in the
resulting pipelines. This seperation means syntax highlighting and
extra tooling works for bash scripts.

`gocd/templates/jsonnetfile.json` and `gocd/templates/jsonnetfile.lock.json`
are used by [jsonnet-bundler](https://github.com/jsonnet-bundler/jsonnet-bundler#readme), a package manager for jsonnet.

You can update jsonnet dependencies by runnning `jb update`.

### `gocd/generated-pipelines/`

GoCD generates pipelines using the jsonnet files directly, hence why
the generated pipelines are part of the gitignore.
