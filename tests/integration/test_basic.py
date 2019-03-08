import pytest
import threading


WINDOWS_DATA = {
    "threads": [
        {"registers": {"eip": "0x0000000001509530"}, "frames": [{"addr": "0x749e8630"}]}
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
}

SUCCESS_WINDOWS = {
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


FAILED_SYMBOLICATION = {
    "errors": [
        "failed to look into cache\n"
        "  caused by: failed to fetch objects\n"
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
            "microsoft.ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1_.pdb": 846_848,
            "microsoft.ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1_.pe": 0,
            "microsoft.ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1_.breakpad": 0,
            "microsoft.ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1_.elf-code": 0,
            "microsoft.ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1_.elf-debug": 0,
            "microsoft.ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1_.mach-code": 0,
            "microsoft.ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1_.mach-debug": 0,
        }

        symcache, = cache_dir_param.join("symcaches").join(stored_in_scope).listdir()
        assert symcache.basename == "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1_"
        assert symcache.size() > 0

    assert hitcounter.hits == {
        "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb": 1
    }

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()

    if cache_dir_param:
        assert hitcounter.hits == {
            "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb": 1
        }
    else:
        assert hitcounter.hits == {
            "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb": 2
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
