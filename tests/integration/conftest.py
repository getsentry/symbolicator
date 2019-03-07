import subprocess
import time
import socket
import os
import json
import pytest
import requests
import threading

from pytest_localserver.http import WSGIServer

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


@pytest.fixture
def hitcounter(request):
    errors = []
    hits = {}
    hitlock = threading.Lock()

    def app(environ, start_response):
        try:
            path = environ["PATH_INFO"]
            with hitlock:
                hits.setdefault(path, 0)
                hits[path] += 1

            if path.startswith("/msdl/"):
                path = path[len("/msdl/") :]
                with requests.get(
                    f"https://msdl.microsoft.com/download/symbols/{path}"
                ) as r:
                    start_response(f"{r.status_code} BOGUS", list(r.headers.items()))
                    yield r.content
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
    return HitCounter(url=server.url, hits=hits)
