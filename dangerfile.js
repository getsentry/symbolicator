function getCleanTitle() {
  const title = danger.github.pr.title;
  return title.split(": ").slice(-1)[0].trim().replace(/\.+$/, "");
}

async function checkChangelog(path) {
  const contents = await danger.github.utils.fileContents(path);
  return contents.includes(prLink);
}

function getChangelogDetails() {
  const prNumber = danger.github.pr.number;
  const prUrl = danger.github.pr.html_url;
  const prLink = `[#${prNumber}](${prUrl})`;

  return `
<details>
<summary><b>Instructions and example for changelog</b></summary>

Please add an entry to \`CHANGELOG.md\` to the "Unreleased" section under the following heading:
 1. **Features**: For new user-visible functionality.
 2. **Bug Fixes**: For user-visible bug fixes.
 3. **Internal**: For features and bug fixes in internal operation.

To the changelog entry, please add a link to this PR (consider a more descriptive message):

\`\`\`md
- ${getCleanTitle()}. (${prLink})
\`\`\`

If none of the above apply, you can opt out by adding _#skip-changelog_ to the PR description.

</details>
`;
}

function checkChangelog() {
  const hasChangelog = danger.git.modified_files.indexOf("CHANGELOG.md") !== -1;
  const skipChangelog =
    danger.github && (danger.github.pr.body + "").includes("#skip-changelog");

  if (!skipChangelog && !hasChangelog) {
    fail("Please consider adding a changelog entry for the next release.");
    markdown(getChangelogDetails());
  }
}

function getSnapshotDetails() {
  return `
<details>
<summary><b>Instructions for snapshot changes</b></summary>

Sentry contains a separate symbolicator integration test suite located at
[\`tests/symbolicator/\`](https://github.com/getsentry/sentry/tree/master/tests/symbolicator).
Changes in this PR will likely result in snapshot diffs in Sentry, which will break the master
branch and in-progress PRs.

Follow these steps to update snapshots in Sentry:

1. Check out latest Sentry \`master\` and enable the virtualenv.
2. Stop the symbolicator devservice using \`sentry devservices down symbolicator\`.
3. Run your development symbolicator on port ``3021``.
4. Export \`SENTRY_SNAPSHOTS_WRITEBACK=1\` and run symbolicator tests with pytest.
5. Review snapshot changes locally, then create a PR to Sentry.
6. Merge the Symbolicator PR, then merge the Sentry PR.

</details>
  `;
}

function checkSnapshots() {
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

function checkAll() {
  const isDraft = danger.github && danger.github.pr.mergeable_state === "draft";

  if (!isDraft) {
    checkChangelog();
    checkSnapshots();
  }
}

checkAll();
