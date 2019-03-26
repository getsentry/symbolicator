import pytest
import time
import threading

WINDOWS_DATA = {
    "threads": [
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
    "errors": [],
    "stacktraces": [
        {
            "frames": [
                {
                    "original_index": 0,
                    "instruction_addr": "0x749e8630",
                    "filename": None,
                    "lang": None,
                    "lineno": 0,
                    "abs_path": None,
                    "package": "C:\\Windows\\System32\\kernel32.dll",
                    "function": "@BaseThreadInitThunk@12",
                    "symbol": "@BaseThreadInitThunk@12",
                    "sym_addr": "0x749e8630",
                }
            ]
        }
    ],
    "status": "completed",
}


FAILED_SYMBOLICATION = {
    "errors": [
        {
            "data": (
                "failed to look into cache\n"
                "  caused by: failed to fetch objects\n"
                "  caused by: no symbols found"
            ),
            "type": "nativeMissingDsym",
        },
        {
            "data": "Failed to symbolicate addr 0x749e8630: no debug file found for address",
            "type": "nativeMissingDsym",
        },
    ],
    "stacktraces": [
        {
            "frames": [
                {
                    "original_index": 0,
                    "instruction_addr": "0x749e8630",
                    "filename": None,
                    "abs_path": None,
                    "lang": None,
                    "lineno": None,
                    "package": None,
                    "function": None,
                    "symbol": None,
                    "sym_addr": None,
                }
            ]
        }
    ],
    "status": "completed",
}


@pytest.fixture(params=[True, False])
def cache_dir_param(tmpdir, request):
    if request.param:
        return tmpdir.mkdir("caches")


@pytest.mark.parametrize("is_public", [True, False])
def test_basic(symbolicator, cache_dir_param, is_public, hitcounter):
    scope = "myscope"

    input = dict(
        meta={"arch": "x86", "scope": scope},
        **WINDOWS_DATA,
        sources=[
            {
                "type": "http",
                "id": "microsoft",
                "layout": "symstore",
                "filetypes": ["pdb", "pe"],
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

        response = service.post("/symbolicate", json=input)
        response.raise_for_status()

        assert response.json() == SUCCESS_WINDOWS

        if cache_dir_param:
            stored_in_scope = "global" if is_public else scope
            assert {
                o.basename: o.size()
                for o in cache_dir_param.join("objects").join(stored_in_scope).listdir()
            } == {
                "microsoft_ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1__pdb": 846_848,
                "microsoft_ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1__pe": 0,
                "microsoft_ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1__breakpad": 0,
                "microsoft_ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1__elf-code": 0,
                "microsoft_ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1__elf-debug": 0,
                "microsoft_ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1__mach-code": 0,
                "microsoft_ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1__mach-debug": 0,
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
    input = dict(meta={"arch": "x86", "scope": "myscope"}, **WINDOWS_DATA, sources=[])

    service = symbolicator(cache_dir=cache_dir_param)
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()

    assert response.json() == FAILED_SYMBOLICATION

    if cache_dir_param:
        assert not cache_dir_param.join("objects/global").exists()
        assert not cache_dir_param.join("symcaches/global").listdir()


@pytest.mark.parametrize("is_public", [True, False])
def test_lookup_deduplication(symbolicator, hitcounter, is_public):
    input = {
        "meta": {"arch": "x86", "scope": "myscope"},
        "modules": WINDOWS_DATA["modules"],
        "sources": [
            {
                "type": "http",
                "id": "microsoft",
                "filetypes": ["pdb", "pe"],
                "layout": "symstore",
                "url": f"{hitcounter.url}/msdl/",
                "is_public": is_public,
            }
        ],
    }

    service = symbolicator(cache_dir=None)
    service.wait_healthcheck()

    def f():
        response = service.post("/symbolicate", json=input)
        response.raise_for_status()
        assert response.json() == {
            "errors": [],
            "stacktraces": [],
            "status": "completed",
        }

    ts = []
    for _ in range(20):
        t = threading.Thread(target=f)
        t.start()
        ts.append(t)

    for t in ts:
        t.join()

    assert hitcounter.hits == {
        "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb": 1
    }


def test_sources_without_filetypes(symbolicator, hitcounter):
    input = dict(
        meta={"arch": "x86", "scope": "myscope"},
        sources=[
            {
                "type": "http",
                "id": "microsoft",
                "filetypes": [],
                "layout": "symstore",
                "url": f"{hitcounter.url}/msdl/",
            }
        ],
        **WINDOWS_DATA,
    )

    service = symbolicator()
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()

    assert response.json() == FAILED_SYMBOLICATION
    assert not hitcounter.hits


@pytest.mark.parametrize("predefined_request_id", [None, "123"])
def test_timeouts(symbolicator, hitcounter, predefined_request_id):
    hitcounter.before_request = lambda: time.sleep(3)

    input = dict(
        meta={"arch": "x86", "scope": "myscope"},
        request={"timeout": 1, "request_id": predefined_request_id},
        sources=[
            {
                "type": "http",
                "id": "microsoft",
                "filetypes": ["pdb", "pe"],
                "layout": "symstore",
                "url": f"{hitcounter.url}/msdl/",
            }
        ],
        **WINDOWS_DATA,
    )

    responses = []

    service = symbolicator()
    service.wait_healthcheck()

    for _ in range(10):
        response = service.post("/symbolicate", json=input)
        response.raise_for_status()
        response = response.json()
        responses.append(response)
        if response["status"] == "completed":
            break
        elif response["status"] == "pending":
            if predefined_request_id:
                assert predefined_request_id == response["request_id"]
            else:
                input["request"]["request_id"] = response["request_id"]
        else:
            assert False

    for response in responses[:-1]:
        assert response["status"] == "pending"
        assert response["request_id"] == input["request"]["request_id"]

    assert responses[-1] == SUCCESS_WINDOWS
    assert len(responses) > 1

    assert hitcounter.hits == {
        "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb": 1
    }
