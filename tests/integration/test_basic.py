import pytest
import threading


@pytest.fixture(params=[True, False])
def cache_dir_param(tmpdir, request):
    if request.param:
        return tmpdir.mkdir("caches")


@pytest.mark.parametrize("is_public", [True, False])
def test_basic(symbolicator, cache_dir_param, is_public):
    scope = "myscope"

    input = {
        "meta": {"arch": "x86", "scope": scope},
        "threads": [
            {
                "registers": {"eip": "0x0000000001509530"},
                "frames": [{"addr": "0x749e8630"}],
            }
        ],
        "modules": [
            {
                "debug_id": "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1",
                "code_name": "C:\\Windows\\System32\\kernel32.dll",
                "debug_name": "C:\\Windows\\System32\\wkernel32.pdb",
                "type": "pe",
                "address": "0x749d0000",
                "size": 851_968,
            }
        ],
        "sources": [
            {
                "type": "http",
                "id": "microsoft",
                "url": "https://msdl.microsoft.com/download/symbols/",
                "is_public": is_public,
            }
        ],
    }

    service = symbolicator(cache_dir=cache_dir_param)
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()

    assert response.json() == {
        "errors": [],
        "stacktraces": [
            {
                "frames": [
                    {
                        "addr": "0x749e8630",
                        "file": None,
                        "language": None,
                        "line": None,
                        "line_address": None,
                        "module": None,
                        "name": "@BaseThreadInitThunk@12",
                        "symbol": "@BaseThreadInitThunk@12",
                        "symbol_address": None,
                    }
                ]
            }
        ],
        "status": "completed",
    }

    if cache_dir_param:
        stored_in_scope = "global" if is_public else scope
        object, = cache_dir_param.join("objects").join(stored_in_scope).listdir()
        assert (
            object.basename
            == "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1--wkernel32.pdb-kernel32.dll"
        )
        assert object.size() > 0

        symcache, = cache_dir_param.join("symcaches").join(stored_in_scope).listdir()
        assert (
            symcache.basename
            == "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1--wkernel32.pdb-kernel32.dll"
        )
        assert symcache.size() > 0


def test_missing_symbols(symbolicator, cache_dir_param):
    input = {
        "meta": {"arch": "x86", "scope": "myscope"},
        "threads": [
            {
                "registers": {"eip": "0x0000000001509530"},
                "frames": [{"addr": "0x749e8630"}],
            }
        ],
        "modules": [
            {
                "debug_id": "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1",
                "code_name": "C:\\Windows\\System32\\kernel32.dll",
                "debug_name": "C:\\Windows\\System32\\wkernel32.pdb",
                "type": "pe",
                "address": "0x749d0000",
                "size": 851_968,
            }
        ],
        "sources": [],
    }

    service = symbolicator(cache_dir=cache_dir_param)
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()

    assert response.json() == {
        "errors": [
            "failed to look into cache\n"
            "  caused by: failed to parse symcache during download\n"
            "  caused by: no symbols found",
            "Failed to symbolicate addr 0x749e8630: symbol not found",
        ],
        "stacktraces": [
            {
                "frames": [
                    {
                        "addr": "0x749e8630",
                        "file": None,
                        "language": None,
                        "line": None,
                        "line_address": None,
                        "module": None,
                        "name": None,
                        "symbol": None,
                        "symbol_address": None,
                    }
                ]
            }
        ],
        "status": "completed",
    }

    if cache_dir_param:
        object, = cache_dir_param.join("objects/global").listdir()
        assert (
            object.basename
            == "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1--wkernel32.pdb-kernel32.dll"
        )
        assert object.size() == 0

        assert not cache_dir_param.join("symcaches/global").listdir()


@pytest.mark.parametrize("is_public", [True, False])
def test_lookup_deduplication(symbolicator, hitcounter, is_public):
    input = {
        "meta": {"arch": "x86", "scope": "myscope"},
        "modules": [
            {
                "debug_id": "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1",
                "code_name": "C:\\Windows\\System32\\kernel32.dll",
                "debug_name": "C:\\Windows\\System32\\wkernel32.pdb",
                "type": "pe",
                "address": "0x749d0000",
                "size": 851_968,
            }
        ],
        "sources": [
            {
                "type": "http",
                "id": "microsoft2",
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
