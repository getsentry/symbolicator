import pytest


@pytest.fixture(params=[True, False])
def cache_dir_param(tmpdir, request):
    if request.param:
        return tmpdir.mkdir("caches")


def test_basic(symbolicator, cache_dir_param):
    input = {
        "meta": {"arch": "x86"},
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
                "size": 851968,
            }
        ],
        "sources": [
            {
                "type": "http",
                "id": "microsoft",
                "url": "https://msdl.microsoft.com/download/symbols/",
                "scope": "global",
            }
        ],
    }

    service = symbolicator(cache_dir=cache_dir_param)
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)

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
        object, = cache_dir_param.join("objects/global").listdir()
        assert (
            object.basename
            == "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1--wkernel32.pdb-kernel32.dll"
        )
        assert object.size() > 0

        symcache, = cache_dir_param.join("symcaches/global").listdir()
        assert (
            symcache.basename
            == "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1--wkernel32.pdb-kernel32.dll"
        )
        assert symcache.size() > 0


def test_missing_symbols(symbolicator, cache_dir_param):
    input = {
        "meta": {"arch": "x86"},
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
                "size": 851968,
            }
        ],
        "sources": [],
    }

    service = symbolicator(cache_dir=cache_dir_param)
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)

    assert response.json() == {
        "errors": [
            "Failed to get symcache: Failed to fetch objects: No symbols found",
            "Failed to symbolicate addr 0x749e8630: Symbol not found",
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
