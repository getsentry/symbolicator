import subprocess
import uuid
import time
import socket
import os
import json
import pytest
import requests
import threading
import boto3

from pytest_localserver.http import WSGIServer

SYMBOLICATOR_BIN = [os.environ.get("SYMBOLICATOR_BIN") or "target/debug/symbolicator"]

AWS_ACCESS_KEY_ID = os.environ.get("SENTRY_SYMBOLICATOR_TEST_AWS_ACCESS_KEY_ID")
AWS_SECRET_ACCESS_KEY = os.environ.get("SENTRY_SYMBOLICATOR_TEST_AWS_SECRET_ACCESS_KEY")
AWS_REGION_NAME = "us-east-1"

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
                if backoff > 10:
                    raise
                backoff *= 2

    def wait_healthcheck(self):
        self.wait_http("/healthcheck")


class Symbolicator(Service):
    pass


@pytest.fixture
def symbolicator(tmpdir, request, random_port, background_process):
    def inner(**config_data):
        config = tmpdir.join("config")
        port = random_port()
        bind = f"127.0.0.1:{port}"

        config_data["bind"] = bind
        config_data["logging"] = {"level": "debug"}

        if config_data.get("cache_dir"):
            config_data["cache_dir"] = str(config_data["cache_dir"])

        config.write(json.dumps(config_data))
        process = background_process(SYMBOLICATOR_BIN + ["-c", str(config), "run"])
        return Symbolicator(process=process, port=port)

    return inner


class HitCounter:
    def __init__(self, url, hits):
        self.url = url
        self.hits = hits
        self.before_request = None


@pytest.fixture
def hitcounter(request):
    errors = []
    hits = {}
    hitlock = threading.Lock()

    rv = None

    def app(environ, start_response):
        if rv.before_request:
            rv.before_request()

        try:
            path = environ["PATH_INFO"]
            with hitlock:
                hits.setdefault(path, 0)
                hits[path] += 1

            if path.startswith("/redirect/"):
                path = path[len("/redirect") :]
                start_response("302 Found", [("Location", path)])
                yield b""
            elif path.startswith("/msdl/"):
                path = path[len("/msdl/") :]

                with requests.get(
                    f"https://msdl.microsoft.com/download/symbols/{path}",
                    allow_redirects=False  # test redirects with msdl
                ) as r:
                    start_response(f"{r.status_code} BOGUS", list(r.headers.items()))
                    yield r.content
            elif path.startswith("/respond_statuscode/"):
                statuscode = int(path.split("/")[2])
                start_response(f"{statuscode} BOGUS", [])
                yield b""

            elif path.startswith("/garbage_data/"):
                start_response("200 OK", [])
                yield b"bogus"
            else:
                raise AssertionError("Bad path: {}".format(path))
        except Exception as e:
            errors.append(e)

    @request.addfinalizer
    def _():
        for error in errors:
            raise error

    server = WSGIServer(application=app, threaded=True)
    server.start()
    request.addfinalizer(server.stop)
    rv = HitCounter(url=server.url, hits=hits)
    return rv


@pytest.fixture
def s3():
    if not AWS_ACCESS_KEY_ID or not AWS_SECRET_ACCESS_KEY:
        pytest.skip("No AWS credentials")
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
