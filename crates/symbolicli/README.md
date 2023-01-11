`symbolicli` is a cli utility that lets you symbolicate native events and minidumps locally.

By default, `symbolicli` will target https://sentry.io/. If you are hosting your own
Sentry instance, you can override this with the `--url` option or the `url` config setting.

# Basic Usage
```
symbolicli -o <ORG> -p <PROJECT> --auth-token <TOKEN> <EVENT>
```

* `<ORG>` and `<PROJECT>` are the organization and project to which the event belongs;
* `<TOKEN>` is a Sentry authentication token that has access to the project;
* `<EVENT>` is either a local file (minidump or event JSON) or the ID of an event from the Sentry instance.

Alternatively, you can run `symbolicli` in offline mode:
```
symbolicli --offline <EVENT>
```

In offline mode `symbolicli` will not attempt to access a Sentry server, which means you can only
process local events.

# Configuration

`symbolicli` can be configured via the `~/.symboliclirc` config file, written in the TOML format.
You can also place a `.symboliclirc` or `symbolicli.toml` file in a project directory to override
global settings.

The available options are:

* `url`: The base URL of the sentry instance. Defaults to `https://sentry.io/`.
* `auth_token`: A Sentry authentication token. This can be overridden with the `SENTRY_AUTH_TOKEN`
  environment variable or the `--auth-token` command line option.
* `org`: The default organization for which to process events. This can be overridden with the `--org`
  command line option.
* `project`: The default project for which to process events. This can be overridden with the `--project`
  command line option.
* `sources`: A list of debug file sources that will be queried in addition to the project's uploaded
  files.