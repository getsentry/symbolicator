function getCleanTitle() {
  const title = danger.github.pr.title;
  return title.split(": ").slice(-1)[0].trim().replace(/\.+$/, "");
}

async function checkChangelog(path) {
  const contents = await danger.github.utils.fileContents(path);
  return contents.includes(prLink);
}

function getChangelogMessage() {
  const prNumber = danger.github.pr.number;
  const prUrl = danger.github.pr.html_url;
  const prLink = `[#${prNumber}](${prUrl})`;

  return `
Please add an entry to \`CHANGELOG.md\` to the "Unreleased" section under the following heading:
 1. **Features**: For new user-visible functionality.
 2. **Bug Fixes**: For user-visible bug fixes.
 3. **Internal**: For features and bug fixes in internal operation.

To the changelog entry, please add a link to this PR (consider a more descriptive message):

\`\`\`md
- ${getCleanTitle()}. (${prLink})
\`\`\`

If none of the above apply, you can opt out by adding _#skip-changelog_ to the PR description.
`;
}

function checkChangelog() {
  const hasChangelog = danger.git.modified_files.indexOf("CHANGELOG.md") !== -1;
  const skipChangelog =
    danger.github && (danger.github.pr.body + "").includes("#skip-changelog");

  if (!skipChangelog && !hasChangelog) {
    fail("Please consider adding a changelog entry for the next release.");
    markdown(getChangelogMessage());
  }
}

function checkSnapshots() {
  const SNAPSHOT_LOCATION = "src/actors/snapshots/";
  const changesSnapshots =
    danger.git.modified_files.indexOf(SNAPSHOT_LOCATION) !== -1;

  if (changesSnapshots) {
    warn(
      "Snapshot changes likely affect Sentry tests. Please check the symbolicator test suite in " +
        "Sentry and update snapshots as needed."
    );
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
