local symbolicator = import './pipelines/symbolicator.libsonnet';
local pipedream = import 'github.com/getsentry/gocd-jsonnet/v1.0.0/pipedream.libsonnet';

local pipedream_config = {
  // Name of your service
  name: 'symbolicator-next',

  // The materials you'd like the pipelines to watch for changes
  materials: {
    init_repo: {
      git: 'git@github.com:getsentry/symbolicator.git',
      shallow_clone: true,
      branch: 'master',
      destination: 'init',
    },
  },

  // Set to true to auto-deploy changes (defaults to true)
  auto_deploy: false,
  // Set to true if you want each pipeline to require manual approval
  auto_pipeline_progression: false,
};

// Then call pipedream.render() to generate the set of pipelines for
// a getsentry "pipedream".
pipedream.render(pipedream_config, symbolicator)
