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
2. Stop the symbolicator devservice using \`sentry devservices down symbolicator\`.
3. Run your development symbolicator on port \`3021\`.
4. Export \`SENTRY_SNAPSHOTS_WRITEBACK=1\` and run symbolicator tests with pytest.
5. Review snapshot changes locally, then create a PR to Sentry.
6. Merge the Symbolicator PR, then merge the Sentry PR.

</details>
  `;
}

async function checkSnapshots() {
  const SNAPSHOT_LOCATION = "src/actors/snapshots/";
  const changesSnapshots = danger.git.modified_files.some((f) =>
    f.startsWith(SNAPSHOT_LOCATION)
  );

  if (changesSnapshots) {
    warn(
      "Snapshot changes likely affect Sentry tests. Please check the symbolicator test suite in " +
        "Sentry and update snapshots as needed."
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
