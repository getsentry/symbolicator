import copy
import threading
import time

import pytest

from conftest import assert_symbolication


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
            "type": "pe",
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
            "registers": {"eip": "0x1509530"},
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
            ],
        }
    ],
    "modules": [
        {
            "type": "pe",
            "debug_id": "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1",
            "code_file": "C:\\Windows\\System32\\kernel32.dll",
            "debug_file": "C:\\Windows\\System32\\wkernel32.pdb",
            "debug_status": "found",
            "features": {
                "has_debug_info": True,
                "has_sources": False,
                "has_symbols": True,
                "has_unwind_info": True,
            },
            "arch": "x86",
            "image_addr": "0x749d0000",
            "image_size": 851_968,
            "candidates": [
                {
                    "download": {"status": "notfound"},
                    "location": "http://127.0.0.1:1234/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pd_",
                    "source": "microsoft",
                },
                {
                    "debug": {"status": "ok"},
                    "download": {
                        "features": {
                            "has_debug_info": True,
                            "has_sources": False,
                            "has_symbols": True,
                            "has_unwind_info": True,
                        },
                        "status": "ok",
                    },
                    "location": "http://127.0.0.1:1234/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb",
                    "source": "microsoft",
                },
            ],
        }
    ],
    "status": "completed",
}


def _make_unsuccessful_result(status, source="microsoft"):
    response = {
        "stacktraces": [
            {
                "registers": {"eip": "0x1509530"},
                "frames": [
                    {
                        "status": status,
                        "original_index": 0,
                        "package": "C:\\Windows\\System32\\kernel32.dll",
                        "instruction_addr": "0x749e8630",
                    }
                ],
            }
        ],
        "modules": [
            {
                "type": "pe",
                "debug_id": "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1",
                "code_file": "C:\\Windows\\System32\\kernel32.dll",
                "debug_file": "C:\\Windows\\System32\\wkernel32.pdb",
                "debug_status": status,
                "features": {
                    "has_debug_info": False,
                    "has_sources": False,
                    "has_symbols": False,
                    "has_unwind_info": False,
                },
                "arch": "unknown",
                "image_addr": "0x749d0000",
                "image_size": 851_968,
            }
        ],
        "status": "completed",
    }
    if source in ["microsoft", "unknown", "broken"]:
        response["modules"][0]["candidates"] = [
            {
                "download": {"status": "notfound"},
                "location": "http://127.0.0.1:1234/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pd_",
                "source": source,
            },
            {
                "download": {"status": "notfound"},
                "location": "http://127.0.0.1:1234/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb",
                "source": source,
            },
        ]
    return response


MISSING_FILE = _make_unsuccessful_result("missing")
MALFORMED_FILE = _make_unsuccessful_result("malformed")
MALFORMED_NO_SOURCES = _make_unsuccessful_result("malformed", source=None)
NO_SOURCES = _make_unsuccessful_result("missing", source=None)
UNKNOWN_SOURCE = _make_unsuccessful_result("missing", source="unknown")


@pytest.fixture(params=[True, False])
def cache_dir_param(tmpdir, request):
    if request.param:
        return tmpdir.mkdir("caches")


@pytest.mark.parametrize(
    "is_public", [True, False], ids=["global_cache", "local_cache"]
)
def test_basic_windows(symbolicator, cache_dir_param, is_public, hitcounter):
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
        options={
            "dif_candidates": True,
        },
    )

    # i = 0: Cache miss
    # i = 1: Cache hit
    # i = 2: Assert that touching the file during cache hit did not destroy the cache
    for i in range(3):
        service = symbolicator(cache_dir=cache_dir_param)
        service.wait_healthcheck()

        response = service.post(f"/symbolicate?scope={scope}", json=input)
        response.raise_for_status()

        assert_symbolication(response.json(), SUCCESS_WINDOWS)

        if cache_dir_param:
            stored_in_scope = "global" if is_public else scope
            assert {
                o.basename: o.size()
                for o in cache_dir_param.join("objects").join(stored_in_scope).listdir()
            } == {
                "microsoft_wkernel32_pdb_FF9F9F7841DB88F0CDEDA9E1E9BFF3B51_wkernel32_pd_": 0,
                "microsoft_wkernel32_pdb_FF9F9F7841DB88F0CDEDA9E1E9BFF3B51_wkernel32_pdb": 846_848,
            }

            (symcache,) = (
                cache_dir_param.join("symcaches").join(stored_in_scope).listdir()
            )
            assert (
                symcache.basename
                == "microsoft_wkernel32_pdb_FF9F9F7841DB88F0CDEDA9E1E9BFF3B51_wkernel32_pdb"
            )
            assert symcache.size() > 0

        if cache_dir_param:
            hit_count = miss_count = 1
        else:
            miss_count = i + 1
            # XXX(markus): Symbolicator opens a cachefile twice if it maps
            # successfully. With caches this doesn't matter, but without caches
            # enabled Symbolicator effectively downloads every item twice
            hit_count = 2 * (i + 1)

        assert hitcounter.hits == {
            "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pd_": miss_count,
            "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb": hit_count,
        }


def test_no_sources(symbolicator, cache_dir_param):
    input = dict(**WINDOWS_DATA, sources=[])

    service = symbolicator(cache_dir=cache_dir_param)
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()

    assert_symbolication(response.json(), NO_SOURCES)

    if cache_dir_param:
        assert not cache_dir_param.join("objects/global").exists()
        assert not cache_dir_param.join("symcaches/global").exists()


def test_unknown_field(symbolicator, cache_dir_param):
    service = symbolicator(cache_dir=cache_dir_param)
    service.wait_healthcheck()

    request = dict(
        **WINDOWS_DATA,
        sources=[],  # Disable for faster test result. We don't care about symbolication
        unknown="value",  # Should be ignored
    )

    response = service.post("/symbolicate", json=request)
    assert response.status_code == 200


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
        options={
            "dif_candidates": True,
        },
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

    for response in responses:
        assert_symbolication(response, SUCCESS_WINDOWS)

    assert set(hitcounter.hits) == {
        "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pd_",
        "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb",
    }

    for key, count in hitcounter.hits.items():
        assert count < 20, (key, count)


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
        options={
            "dif_candidates": True,
        },
        **WINDOWS_DATA,
    )
    expected = copy.deepcopy(NO_SOURCES)
    expected["modules"][0]["candidates"] = [
        {
            "source": "microsoft",
            "location": "No object files listed on this source",
            "download": {
                "status": "notfound",
            },
        }
    ]

    service = symbolicator()
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()

    assert_symbolication(response.json(), expected)
    assert not hitcounter.hits


def test_unknown_source_config(symbolicator, hitcounter):
    # Requests could contain invalid data which should not stop symbolication,
    # name is sent by Sentry and is unknown.
    input = dict(
        sources=[
            {
                "type": "http",
                "id": "unknown",
                "layout": {"type": "symstore"},
                "url": f"{hitcounter.url}/respond_statuscode/400",
                "name": "not a known field",
                "not-a-field": "more unknown fields",
            }
        ],
        options={
            "dif_candidates": True,
        },
        **WINDOWS_DATA,
    )

    service = symbolicator()
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()

    expected = copy.deepcopy(UNKNOWN_SOURCE)
    for module in expected.get("modules", []):
        for candidate in module.get("candidates", []):
            if "location" in candidate:
                candidate["location"] = candidate["location"].replace(
                    "/msdl/",
                    "/respond_statuscode/400/",
                )
    assert_symbolication(response.json(), expected)


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
                options={
                    "dif_candidates": True,
                },
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
        time.sleep(0.3)  # 0.3 * 10 iterations = 3s => expect timeout on 4th iteration

    for response in responses[:-1]:
        assert response["status"] == "pending"
        assert response["request_id"] == request_id

    assert_symbolication(responses[-1], SUCCESS_WINDOWS)
    assert len(responses) > 1

    assert hitcounter.hits == {
        "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pd_": 1,
        # XXX(markus): Symbolicator opens a cachefile twice if it maps
        # successfully. With caches this doesn't matter, but without caches
        # enabled Symbolicator effectively downloads every item twice
        "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb": 2,
    }


@pytest.mark.parametrize("bucket_type", ["http", "sentry"])
@pytest.mark.parametrize("statuscode", [400, 500, 404])
def test_unreachable_bucket(symbolicator, hitcounter, statuscode, bucket_type):
    input = dict(
        sources=[
            {
                "type": bucket_type,
                "id": "broken",
                "layout": {"type": "symstore"},  # only relevant for http type
                "url": f"{hitcounter.url}/respond_statuscode/{statuscode}/",
                "token": "123abc",  # only relevant for sentry type
            }
        ],
        options={
            "dif_candidates": True,
        },
        **WINDOWS_DATA,
    )

    service = symbolicator()
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()
    response = response.json()
    # TODO(markus): Better error reporting
    if bucket_type == "sentry":
        expected = copy.deepcopy(NO_SOURCES)
        expected["modules"][0]["candidates"] = [
            {
                "source": "broken",
                "location": "No object files listed on this source",
                "download": {
                    "status": "notfound",
                },
            }
        ]
        assert_symbolication(response, expected)
    else:
        expected = _make_unsuccessful_result(status="missing", source="broken")
        for module in expected.get("modules", []):
            for candidate in module.get("candidates", []):
                if "location" in candidate:
                    candidate["location"] = candidate["location"].replace(
                        "/msdl/",
                        f"/respond_statuscode/{statuscode}/",
                    )

        assert_symbolication(response, expected)


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
        options={
            "dif_candidates": True,
        },
        **WINDOWS_DATA,
    )

    service = symbolicator()
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()
    response = response.json()
    assert_symbolication(response, MALFORMED_NO_SOURCES)


@pytest.mark.parametrize(
    "patterns,output",
    [
        [["?:/windows/**"], SUCCESS_WINDOWS],
        [["?:/windows/*"], SUCCESS_WINDOWS],
        [[], SUCCESS_WINDOWS],
        [["?:/windows/"], NO_SOURCES],
        [["d:/windows/**"], NO_SOURCES],
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
        options={
            "dif_candidates": True,
        },
        **WINDOWS_DATA,
    )
    if output == NO_SOURCES:
        output = copy.deepcopy(output)
        output["modules"][0]["candidates"] = [
            {
                "source": "microsoft",
                "location": "No object files listed on this source",
                "download": {
                    "status": "notfound",
                },
            }
        ]

    service = symbolicator()
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()

    assert_symbolication(response.json(), output)


def test_redirects(symbolicator, hitcounter):
    input = dict(
        sources=[
            {
                "type": "http",
                "id": "microsoft",
                "layout": {"type": "symstore"},
                "url": f"{hitcounter.url}/redirect/msdl/",
            }
        ],
        options={
            "dif_candidates": True,
        },
        **WINDOWS_DATA,
    )

    service = symbolicator()
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()

    expected = copy.deepcopy(SUCCESS_WINDOWS)
    for module in expected.get("modules", []):
        for candidate in module.get("candidates", []):
            if "location" in candidate:
                candidate["location"] = candidate["location"].replace(
                    "/msdl/", "/redirect/msdl/"
                )

    assert_symbolication(response.json(), expected)


@pytest.mark.parametrize("allow_reserved_ip", [True, False])
@pytest.mark.parametrize("hostname", ["dev.getsentry.net", "localhost", "127.0.0.1"])
def test_reserved_ip_addresses(symbolicator, hitcounter, allow_reserved_ip, hostname):
    service = symbolicator(connect_to_reserved_ips=allow_reserved_ip)
    service.wait_healthcheck()

    url = hitcounter.url.replace("localhost", hostname).replace("127.0.0.1", hostname)
    assert hostname in url

    input = dict(
        sources=[
            {
                "type": "http",
                "id": "microsoft",
                "layout": {"type": "symstore"},
                "url": f"{url}/msdl/",
            }
        ],
        options={
            "dif_candidates": True,
        },
        **WINDOWS_DATA,
    )

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()

    if allow_reserved_ip:
        assert hitcounter.hits
        assert_symbolication(response.json(), SUCCESS_WINDOWS)
    else:
        assert not hitcounter.hits
        assert_symbolication(response.json(), MISSING_FILE)


def test_no_dif_candidates(symbolicator, hitcounter):
    # Asserts that disabling requesting for DIF candidates info works.
    service = symbolicator()
    service.wait_healthcheck()

    request_data = dict(
        **WINDOWS_DATA,
        sources=[
            {
                "type": "http",
                "id": "microsoft",
                "layout": {"type": "symstore"},
                "filters": {"filetypes": ["pdb", "pe"]},
                "url": f"{hitcounter.url}/msdl/",
            }
        ],
    )

    response = service.post("/symbolicate", json=request_data)
    response.raise_for_status()

    success_response = copy.deepcopy(SUCCESS_WINDOWS)
    for module in success_response["modules"]:
        del module["candidates"]

    assert hitcounter.hits
    assert_symbolication(response.json(), success_response)
