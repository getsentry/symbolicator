local symbolicator = import './pipelines/symbolicator.libsonnet';
local pipedream = import 'github.com/getsentry/gocd-jsonnet/libs/pipedream.libsonnet';

local pipedream_config = {
  // Name of your service
  name: 'symbolicator',

  // The materials you'd like the pipelines to watch for changes
  materials: {
    symbolicator_repo: {
      git: 'git@github.com:getsentry/symbolicator.git',
      shallow_clone: true,
      branch: 'master',
      destination: 'symbolicator',
    },
  },

  // Add rollback
  rollback: {
    material_name: 'symbolicator_repo',
    stage: 'deploy_primary',
    elastic_profile_id: 'symbolicator',
  },

  // Set to true to auto-deploy changes (defaults to true)
  auto_deploy: false,
};

// Then call pipedream.render() to generate the set of pipelines for
// a getsentry "pipedream".
pipedream.render(pipedream_config, symbolicator)
