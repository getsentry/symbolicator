import pytest
import time
import threading

WINDOWS_DATA = {
    "signal": None,
    "stacktraces": [
        {
            "registers": {"eip": "0x0000000001509530"},
            "frames": [{"instruction_addr": "0x749e8630"}],
        }
    ],
    "modules": [
        {
            "type": "symbolic",
            "debug_id": "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1",
            "code_file": "C:\\Windows\\System32\\kernel32.dll",
            "debug_file": "C:\\Windows\\System32\\wkernel32.pdb",
            "image_addr": "0x749d0000",
            "image_size": 851_968,
        }
    ],
}

SUCCESS_WINDOWS = {
    "stacktraces": [
        {
            "frames": [
                {
                    "status": "symbolicated",
                    "original_index": 0,
                    "instruction_addr": "0x749e8630",
                    "lineno": 0,
                    "package": "C:\\Windows\\System32\\kernel32.dll",
                    "function": "@BaseThreadInitThunk@12",
                    "symbol": "@BaseThreadInitThunk@12",
                    "sym_addr": "0x749e8630",
                }
            ]
        }
    ],
    "modules": [dict(debug_status="found", arch="x86", **WINDOWS_DATA["modules"][0])],
    "status": "completed",
}


def _make_unsuccessful_result(status):
    return {
        "stacktraces": [
            {
                "frames": [
                    {
                        "status": status,
                        "original_index": 0,
                        "instruction_addr": "0x749e8630",
                    }
                ]
            }
        ],
        "modules": [
            dict(debug_status=status, arch="unknown", **WINDOWS_DATA["modules"][0])
        ],
        "status": "completed",
    }


MISSING_FILE = _make_unsuccessful_result("missing")
MALFORMED_FILE = _make_unsuccessful_result("malformed")


@pytest.fixture(params=[True, False])
def cache_dir_param(tmpdir, request):
    if request.param:
        return tmpdir.mkdir("caches")


@pytest.mark.parametrize("is_public", [True, False])
def test_basic(symbolicator, cache_dir_param, is_public, hitcounter):
    scope = "myscope"

    input = dict(
        **WINDOWS_DATA,
        sources=[
            {
                "type": "http",
                "id": "microsoft",
                "layout": {"type": "symstore"},
                "filters": {"filetypes": ["pdb", "pe"]},
                "url": f"{hitcounter.url}/msdl/",
                "is_public": is_public,
            }
        ],
    )

    # i = 0: Cache miss
    # i = 1: Cache hit
    # i = 2: Assert that touching the file during cache hit did not destroy the cache
    for i in range(3):
        service = symbolicator(cache_dir=cache_dir_param)
        service.wait_healthcheck()

        response = service.post(f"/symbolicate?scope={scope}", json=input)
        response.raise_for_status()

        assert response.json() == SUCCESS_WINDOWS

        if cache_dir_param:
            stored_in_scope = "global" if is_public else scope
            assert {
                o.basename: o.size()
                for o in cache_dir_param.join("objects").join(stored_in_scope).listdir()
            } == {
                "microsoft_wkernel32_pdb_FF9F9F7841DB88F0CDEDA9E1E9BFF3B51_wkernel32_pdb": 846_848
            }

            symcache, = (
                cache_dir_param.join("symcaches").join(stored_in_scope).listdir()
            )
            assert symcache.basename == "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1_"
            assert symcache.size() > 0

        assert hitcounter.hits == {
            "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb": 1
            if cache_dir_param
            else (i + 1)
        }


def test_no_sources(symbolicator, cache_dir_param):
    input = dict(**WINDOWS_DATA, sources=[])

    service = symbolicator(cache_dir=cache_dir_param)
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()

    assert response.json() == MISSING_FILE

    if cache_dir_param:
        assert not cache_dir_param.join("objects/global").exists()
        symcache, = cache_dir_param.join("symcaches/global").listdir()
        assert symcache.basename == "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1_"
        assert symcache.size() == 0


@pytest.mark.parametrize("is_public", [True, False])
def test_lookup_deduplication(symbolicator, hitcounter, is_public):
    input = dict(
        **WINDOWS_DATA,
        sources=[
            {
                "type": "http",
                "id": "microsoft",
                "filters": {"filetypes": ["pdb", "pe"]},
                "layout": {"type": "symstore"},
                "url": f"{hitcounter.url}/msdl/",
                "is_public": is_public,
            }
        ],
    )

    service = symbolicator(cache_dir=None)
    service.wait_healthcheck()
    responses = []

    def f():
        response = service.post("/symbolicate", json=input)
        response.raise_for_status()
        responses.append(response.json())

    ts = []
    for _ in range(20):
        t = threading.Thread(target=f)
        t.start()
        ts.append(t)

    for t in ts:
        t.join()

    assert responses == [SUCCESS_WINDOWS] * 20

    assert hitcounter.hits == {
        "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb": 1
    }


def test_sources_filetypes(symbolicator, hitcounter):
    input = dict(
        sources=[
            {
                "type": "http",
                "id": "microsoft",
                "filters": {"filetypes": ["elf_code"]},
                "layout": {"type": "symstore"},
                "url": f"{hitcounter.url}/msdl/",
            }
        ],
        **WINDOWS_DATA,
    )

    service = symbolicator()
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()

    assert response.json() == MISSING_FILE
    assert not hitcounter.hits


def test_timeouts(symbolicator, hitcounter):
    hitcounter.before_request = lambda: time.sleep(3)

    request_id = None

    responses = []

    service = symbolicator()
    service.wait_healthcheck()

    for _ in range(10):
        if request_id:
            response = service.get("/requests/{}?timeout=1".format(request_id))
        else:
            input = dict(
                sources=[
                    {
                        "type": "http",
                        "id": "microsoft",
                        "filters": {"filetypes": ["pdb", "pe"]},
                        "layout": {"type": "symstore"},
                        "url": f"{hitcounter.url}/msdl/",
                    }
                ],
                **WINDOWS_DATA,
            )
            response = service.post("/symbolicate?timeout=1", json=input)

        response.raise_for_status()
        response = response.json()
        responses.append(response)
        if response["status"] == "completed":
            break
        elif response["status"] == "pending":
            request_id = response["request_id"]
        else:
            assert False

    for response in responses[:-1]:
        assert response["status"] == "pending"
        assert response["request_id"] == request_id

    assert responses[-1] == SUCCESS_WINDOWS
    assert len(responses) > 1

    assert hitcounter.hits == {
        "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb": 1
    }


@pytest.mark.parametrize("statuscode", [400, 500, 404])
def test_unreachable_bucket(symbolicator, hitcounter, statuscode):
    input = dict(
        sources=[
            {
                "type": "http",
                "id": "broken",
                "layout": {"type": "symstore"},
                "url": f"{hitcounter.url}/respond_statuscode/{statuscode}/",
            }
        ],
        **WINDOWS_DATA,
    )

    service = symbolicator()
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()
    response = response.json()
    # TODO(markus): Better error reporting
    assert response == MISSING_FILE


def test_malformed_objects(symbolicator, hitcounter):
    input = dict(
        sources=[
            {
                "type": "http",
                "id": "broken",
                "layout": {"type": "symstore"},
                "url": f"{hitcounter.url}/garbage_data/",
            }
        ],
        **WINDOWS_DATA,
    )

    service = symbolicator()
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()
    response = response.json()
    assert response == MALFORMED_FILE


@pytest.mark.parametrize(
    "patterns,output",
    [
        [["?:/windows/**"], SUCCESS_WINDOWS],
        [["?:/windows/*"], SUCCESS_WINDOWS],
        [[], SUCCESS_WINDOWS],
        [["?:/windows/"], MISSING_FILE],
        [["d:/windows/**"], MISSING_FILE],
    ],
)
def test_path_patterns(symbolicator, hitcounter, patterns, output):
    input = dict(
        sources=[
            {
                "type": "http",
                "id": "microsoft",
                "layout": {"type": "symstore"},
                "filters": {"path_patterns": patterns},
                "url": f"{hitcounter.url}/msdl/",
            }
        ],
        **WINDOWS_DATA,
    )

    service = symbolicator()
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()

    assert response.json() == output
