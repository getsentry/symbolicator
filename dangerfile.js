const PR_NUMBER = danger.github.pr.number;
const PR_URL = danger.github.pr.html_url;
const PR_LINK = `[#${PR_NUMBER}](${PR_URL})`;

function getCleanTitle() {
  const title = danger.github.pr.title;
  return title.split(": ").slice(-1)[0].trim().replace(/\.+$/, "");
}

function getChangelogDetails() {
  return `
<details>
<summary><b>Instructions and example for changelog</b></summary>

Please add an entry to \`CHANGELOG.md\` to the "Unreleased" section under the following heading:
 1. **Features**: For new user-visible functionality.
 2. **Bug Fixes**: For user-visible bug fixes.
 3. **Internal**: For features and bug fixes in internal operation.

To the changelog entry, please add a link to this PR (consider a more descriptive message):

\`\`\`md
- ${getCleanTitle()}. (${PR_LINK})
\`\`\`

If none of the above apply, you can opt out by adding _#skip-changelog_ to the PR description.

</details>
`;
}

async function containsChangelog(path) {
  const contents = await danger.github.utils.fileContents(path);
  return contents.includes(PR_LINK);
}

async function checkChangelog() {
  const skipChangelog =
    danger.github && (danger.github.pr.body + "").includes("#skip-changelog");

  if (skipChangelog) {
    return;
  }

  const hasChangelog = await containsChangelog("CHANGELOG.md");

  if (!hasChangelog) {
    fail("Please consider adding a changelog entry for the next release.");
    markdown(getChangelogDetails());
  }
}

function getSnapshotDetails() {
  return `
<details>
<summary><b>Instructions for snapshot changes</b></summary>

Sentry runs a symbolicator integration test suite located at [\`tests/symbolicator/\`](https://github.com/getsentry/sentry/tree/master/tests/symbolicator). Changes in this PR will likely result in snapshot diffs in Sentry, which will break the master branch and in-progress PRs.

Follow these steps to update snapshots in Sentry:

1. Check out latest Sentry \`master\` and enable the virtualenv.
2. Enable symbolicator (\`symbolicator: true\`) in sentry via \`~/.sentry/config.yml\`.
3. Make sure your other devservices are running via \`sentry devservices up --exclude symbolicator\`. If
they're already running, stop symbolicator with \`sentry devservices down symbolicator\`. You want to use your
own development symbolicator to update the snapshots.
4. Run your development symbolicator on port \`3021\`, or whatever port symbolicator is configured to use
in \`~/.sentry/config.yml\`.
5. Export \`SENTRY_SNAPSHOTS_WRITEBACK=1\` to automatically update the existing snapshots with your new
results and run symbolicator tests with pytest (\`pytest tests/symbolicator\`).
6. Review snapshot changes locally, then create a PR to Sentry.
7. Merge the Symbolicator PR, then merge the Sentry PR.

</details>
  `;
}

async function checkSnapshots() {
  const SNAPSHOT_LOCATION = "crates/symbolicator/src/services/snapshots/";

  // Sanity check that the snapshot directory exists
  let contents = await danger.github.utils.fileContents(
    SNAPSHOT_LOCATION + "CAUTION.md"
  );
  if (!contents) {
    fail(
      "The snapshot directory has moved to a new location. Please update SNAPSHOT_LOCATION in /dangerfile.js."
    );
    return;
  }

  const changesSnapshots = danger.git.modified_files.some((f) =>
    f.startsWith(SNAPSHOT_LOCATION)
  );

  if (changesSnapshots) {
    warn(
      "Snapshot changes likely affect Sentry tests. If the Sentry-Symbolicator Integration Tests in CI are " +
      "failing for your PR, please check the symbolicator test suite in Sentry and update snapshots as needed."
    );
    markdown(getSnapshotDetails());
  }
}

async function checkAll() {
  // See: https://spectrum.chat/danger/javascript/support-for-github-draft-prs~82948576-ce84-40e7-a043-7675e5bf5690
  const isDraft = danger.github.pr.mergeable_state === "draft";

  if (isDraft) {
    return;
  }

  await checkChangelog();
  await checkSnapshots();
}

schedule(checkAll);
