import collections
import copy
import json
import os
import re
import socket
import subprocess
import threading
import time
import traceback

import pytest
import requests
from pytest_localserver.http import WSGIServer
from werkzeug.serving import WSGIRequestHandler

SYMBOLICATOR_BIN = [os.environ.get("SYMBOLICATOR_BIN") or "target/debug/symbolicator"]

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

        # Required for proxying HTTP/1.1 transfer-encoding "chunked" from the microsoft symbol server.
        # See: https://github.com/hyperium/hyper/blob/48d4594930da4e227039cfa254411b85c98b63c5/src/proto/h1/role.rs#L209
        WSGIRequestHandler.protocol_version = "HTTP/1.1"

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
        elif path.startswith("/symbols/"):
            print(f"got requested: {path}")
            path = path[len("/symbols/") :]
            try:
                filename = os.path.join(
                    os.path.dirname(__file__), "..", "fixtures", "symbols", path
                )
                with open(filename, "rb") as f:
                    d = f.read()
                    start_response("200 OK", [("Content-Length", str(len(d)))])
                    return [d]
            except IOError:
                start_response("404 NOT FOUND", [])
                return [b""]
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


def assert_symbolication(output, expected):
    """Compares symbolication results, with redactions.

    Redactions are necessary to remove random port numbers.
    """
    __tracebackhide__ = True
    output = copy.deepcopy(output)
    expected = copy.deepcopy(expected)

    # dev.getsentry.net gets mapped to 127.0.0.1, that's ugly but simple to use and is what
    # it actually resolves to.
    port_re = re.compile(r"^http://(dev.getsentry.net|localhost|127.0.0.1):[0-9]+")
    s3_bucket_re = re.compile(r"s3://symbolicator-test-[a-f0-9-]+")

    def redact(d):
        for module in d.get("modules", []):
            for candidate in module.get("candidates", []):
                if "location" in candidate:
                    candidate["location"] = port_re.sub(
                        "http://127.0.0.1:<port>", candidate["location"]
                    )
                    candidate["location"] = s3_bucket_re.sub(
                        "s3://symbolicator-tests-<uuid>", candidate["location"]
                    )

    redact(output)
    redact(expected)
    assert output == expected
