const ERROR_MESSAGE =
  "Please consider adding a changelog entry for the next release.";

const DETAILS = `
Please add an entry to \`CHANGELOG.md\` to the "Unreleased" section under the following heading:
 1. **Features**: For new user-visible functionality.
 2. **Bug Fixes**: For user-visible bug fixes.
 3. **Internal**: For features and bug fixes in internal operation.

If none of the above apply, you can opt out by adding _#skip-changelog_ to the PR description.
`;

const files = danger.git.modified_files;
const hasChangelog = files.indexOf("CHANGELOG.md") !== -1;

const skipChangelog =
  danger.github && (danger.github.pr.body + "").includes("#skip-changelog");

if (!skipChangelog && !hasChangelog) {
  fail(ERROR_MESSAGE);
  markdown(DETAILS);
}
