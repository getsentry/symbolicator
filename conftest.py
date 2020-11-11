"""Project-wide pytest customisation.

This module is a symbolicator-specific plugin to pytest, which is used by the integration
tests.  Customisations to the global pytest process should be done here to ensure they are
loaded before any test collection happens.
"""

import resource

import pytest


@pytest.hookimpl
def pytest_sessionstart(session):
    no_fds_soft, no_fds_hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    if no_fds_soft < 2028:
        session.warn(
            pytest.PytestWarning(
                f"Too low open filedescriptor limit: {no_fds_soft} (try `ulimit -n 4096`)"
            )
        )


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_call(item):
    """This handles the extra_failure_checks mark to perform further assertions.

    The mark can be added e.g. by a fixture that wants to run
    something that can raise pytest.fail.Exception after the test has
    completed.

    Such checks will not be run when the test already failed, just
    like multiple assertions within a test will stop the test
    execution at the first failure.
    """
    yield
    for marker in item.iter_markers("extra_failure_checks"):
        for check_func in marker.kwargs.get("checks", []):
            check_func()
