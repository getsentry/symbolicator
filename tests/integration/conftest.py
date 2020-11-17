import collections
import json
import os
import socket
import subprocess
import threading
import time
import traceback
import uuid

import boto3
import pytest
import requests
from pytest_localserver.http import WSGIServer

SYMBOLICATOR_BIN = [os.environ.get("SYMBOLICATOR_BIN") or "target/debug/symbolicator"]

AWS_ACCESS_KEY_ID = os.environ.get("SENTRY_SYMBOLICATOR_TEST_AWS_ACCESS_KEY_ID")
AWS_SECRET_ACCESS_KEY = os.environ.get("SENTRY_SYMBOLICATOR_TEST_AWS_SECRET_ACCESS_KEY")
AWS_REGION_NAME = "us-east-1"
GCS_PRIVATE_KEY = os.environ.get("SENTRY_SYMBOLICATOR_GCS_PRIVATE_KEY")
GCS_CLIENT_EMAIL = os.environ.get("SENTRY_SYMBOLICATOR_GCS_CLIENT_EMAIL")

session = requests.session()


@pytest.fixture
def random_port():
    def inner():
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind(("127.0.0.1", 0))
        s.listen(1)
        port = s.getsockname()[1]
        s.close()
        return port

    return inner


@pytest.fixture
def background_process(request):
    """Function to run a process in the background.

    All args and kwargs are passed to subprocess.Popen and the process is terminated in the
    fixture finaliser.

    The return value of the function is the subprocess.Popen object.
    """

    def inner(*args, **kwargs):
        p = subprocess.Popen(*args, **kwargs)
        request.addfinalizer(p.kill)
        return p

    return inner


class Service:
    def __init__(self, process, port):
        self.process = process
        self.port = port

    @property
    def url(self):
        return f"http://127.0.0.1:{self.port}"

    def request(self, method, path, **kwargs):
        assert path.startswith("/")
        return session.request(method, self.url + path, **kwargs)

    def post(self, path, **kwargs):
        return self.request("post", path, **kwargs)

    def get(self, path, **kwargs):
        return self.request("get", path, **kwargs)

    def wait_http(self, path):
        backoff = 0.1
        while True:
            try:
                self.get(path).raise_for_status()
                break
            except Exception:
                time.sleep(backoff)
                if backoff > 100:  # 10s
                    raise
                backoff += 0.1

    def wait_healthcheck(self):
        self.wait_http("/healthcheck")


class Symbolicator(Service):
    pass


@pytest.fixture
def symbolicator(tmpdir, request, random_port, background_process):
    """Function to run a symbolicator instance.

    All kwargs are written to the config file of the symbolicator instance.

    The return value of the function is a :class:`Symbolicator` object.
    """

    def inner(**config_data):
        config = tmpdir.join("config")
        port = random_port()
        bind = f"127.0.0.1:{port}"

        config_data["bind"] = bind
        config_data["logging"] = {"level": "debug"}
        config_data.setdefault("connect_to_reserved_ips", True)

        if config_data.get("cache_dir"):
            config_data["cache_dir"] = str(config_data["cache_dir"])

        config.write(json.dumps(config_data))
        process = background_process(SYMBOLICATOR_BIN + ["-c", str(config), "run"])
        return Symbolicator(process=process, port=port)

    return inner


class HitCounter:
    """A simple WSGI app which will count the number of times a URL path is served.

    Several URL paths are recognised:

    `/redirect/{tail}`: This redirects to `/{tail}`.

    `/msdl/{tail}`: This proxies the request to
       https://msdl.microsoft.com/download/symbols/{tail}.

    `/respond_statuscode/{num}`: returns and empty response with the given status code.

    `/garbage_data/{tail}`: returns 200 OK with some garbage data in the response body.

    Any other request will return 500 Internal Server Error and will be stored in
    self.errors.

    This object itself is a context manager, when entered the WSGI server will start serving,
    when exited it will stop serving.

    Attributes:

    :ivar url: The URL to reach the server, only available while the server is running.
    :ivar hits: Dictionary of URL paths to hit counters.
    :ivar before_request: Can be optionally set to execute a function before the request is
       handled.
    """

    def __init__(self):
        self.url = None
        self.hits = collections.defaultdict(lambda: 0)
        self.before_request = None
        self._lock = threading.Lock()
        self.errors = []
        self._server = WSGIServer(application=self._app, threaded=True)

    def __enter__(self):
        self._server.start()
        self.url = self._server.url

    def __exit__(self, exc_type, exc_val, exc_tb):
        self._server.stop()
        self.url = None

    def _app(self, environ, start_response):
        """The WSGI app."""
        if self.before_request:
            self.before_request()

        try:
            path = environ["PATH_INFO"]
            with self._lock:
                self.hits[path] += 1
            body = self._handle_path(path, start_response)
        except Exception as e:
            self.errors.append(e)
            start_response("500 Internal Server Error", [])
            return [b"error"]
        else:
            return body

    @staticmethod
    def _handle_path(path, start_response):
        if path.startswith("/redirect/"):
            path = path[len("/redirect") :]
            start_response("302 Found", [("Location", path)])
            return [b""]
        elif path.startswith("/msdl/"):
            print(f"got requested: {path}")
            path = path[len("/msdl/") :]
            print(f"proxying {path}")
            with requests.get(
                f"https://msdl.microsoft.com/download/symbols/{path}",
                allow_redirects=False,  # test redirects with msdl
            ) as r:
                print(f"status code: {r.status_code}")
                start_response(f"{r.status_code} BOGUS", list(r.headers.items()))
                return [r.content]
        elif path.startswith("/respond_statuscode/"):
            statuscode = int(path.split("/")[2])
            start_response(f"{statuscode} BOGUS", [])
            return [b""]

        elif path.startswith("/garbage_data/"):
            start_response("200 OK", [])
            return [b"bogus"]
        else:
            raise AssertionError("Bad path: {}".format(path))


@pytest.fixture
def hitcounter(request):
    """Running HitCounter server.

    This fixture sets up a running hitcounter server and will fail the test if there was any
    error in it's request handling.
    """
    app = HitCounter()

    def fail_on_errors():
        if app.errors:
            tracebacks = [
                "".join(traceback.format_exception(type(exc), exc, None))
                for exc in app.errors
            ]
            pytest.fail(
                "{n} errors in hitcounter server:\n\n{failures}".format(
                    n=len(app.errors), failures="\n\n".join(tracebacks)
                )
            )

    mark = pytest.mark.extra_failure_checks(checks=[fail_on_errors])
    request.node.add_marker(mark)
    with app:
        yield app


def fail_missing_secrets(msg):
    """Fail or skip the test because of missing secrets.

    Secrets are only guaranteed to be available on CI when the branch being tested is
    committed to the main getsentry repository.  Especially for forks they are hidden to
    stop external contributors from stealing the secrets.

    This will call `pytest.skip` if the secrets are missing but this is acceptable because
    e.g. we're on a developer's machine or on a forked repository.  If running on CI in the
    main repository this will call `pytest.fail`.
    """
    if os.getenv("CI") and os.getenv("GITHUB_BASE_REF") is None:
        pytest.fail(msg)
    else:
        pytest.skip(msg)


@pytest.fixture
def s3(pytestconfig):
    """AWS S3 credentials for testing S3 buckets.

    This will skip if the secrets are not in the environment, unless we are running on CI in
    the getsentry team, in which case it will fail.  When running CI not as part of the
    getsentry team you do not automatically get access to the configured secrets so skipping
    is allowed.
    """
    if not AWS_ACCESS_KEY_ID or not AWS_SECRET_ACCESS_KEY:
        fail_missing_secrets("No AWS credentials")
    return boto3.resource(
        "s3",
        aws_access_key_id=AWS_ACCESS_KEY_ID,
        aws_secret_access_key=AWS_SECRET_ACCESS_KEY,
    )


@pytest.fixture
def s3_bucket_config(s3):
    bucket_name = f"symbolicator-test-{uuid.uuid4()}"
    s3.create_bucket(Bucket=bucket_name)

    yield {
        "type": "s3",
        "bucket": bucket_name,
        "access_key": AWS_ACCESS_KEY_ID,
        "secret_key": AWS_SECRET_ACCESS_KEY,
        "region": AWS_REGION_NAME,
    }

    s3.Bucket(bucket_name).objects.all().delete()
    s3.Bucket(bucket_name).delete()


@pytest.fixture
def ios_bucket_config(pytestconfig):
    """Google cloud storage bucket for ios symbols.

    This will skip if the secrets are not in the environment, unless we are running on CI in
    the getsentry team, in which case it will fail.  When running CI not as part of the
    getsentry team you do not automatically get access to the configured secrets so skipping
    is allowed.
    """
    if not GCS_PRIVATE_KEY or not GCS_CLIENT_EMAIL:
        fail_missing_secrets("No GCS credentials")
    yield {
        "id": "ios",
        "type": "gcs",
        "bucket": "sentryio-system-symbols-0",
        "private_key": GCS_PRIVATE_KEY,
        "client_email": GCS_CLIENT_EMAIL,
        "layout": {"type": "unified"},
        "prefix": "/ios",
    }
