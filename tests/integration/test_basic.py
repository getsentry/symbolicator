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

STACKTRACE_RESULT = {
    "registers": {"eip": "0x1509530"},
    "frames": [
        {
            "original_index": 0,
            "package": "C:\\Windows\\System32\\kernel32.dll",
            "instruction_addr": "0x749e8630",
        }
    ],
}

MODULE_RESULT = {
    "type": "pe",
    "debug_id": "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1",
    "code_file": "C:\\Windows\\System32\\kernel32.dll",
    "debug_file": "C:\\Windows\\System32\\wkernel32.pdb",
    "image_addr": "0x749d0000",
    "image_size": 851_968,
}

DEFAULT_SERVER_PATH = "http://127.0.0.1:1234/msdl/"

PATHS = {
    "dwarf": "_.dwarf/mach-uuid-sym-ff9f9f7841db88f0cdeda9e1e9bff3b5/_.dwarf",
    "pd_": "wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pd_",
    "pdb": "wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb",
    "src": "wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.src.zip",
}


def _make_successful_result(filtered=False):
    stacktrace = copy.deepcopy(STACKTRACE_RESULT)
    stacktrace["frames"][0].update(
        {
            "status": "symbolicated",
            "lineno": 0,
            "function": "BaseThreadInitThunk",
            "symbol": "BaseThreadInitThunk",
            "sym_addr": "0x749e8630",
        }
    )

    module = copy.deepcopy(MODULE_RESULT)
    module.update(
        {
            "debug_status": "found",
            "features": {
                "has_debug_info": True,
                "has_sources": False,
                "has_symbols": True,
                "has_unwind_info": True,
            },
            "arch": "x86",
            "candidates": [],
        }
    )

    # only include other debug file types if the request to symbolicator doesn't explicitly filter them out
    # if we filter out the source bundles, symbolicator will create a "not files listed" candidate for them
    if filtered:
        module["candidates"].append(
            {
                "location": "No object files listed on this source",
                "source": "microsoft",
                "download": {"status": "notfound"},
            }
        )
    else:
        module["candidates"].append(
            {
                "location": DEFAULT_SERVER_PATH + PATHS["dwarf"],
                "source": "microsoft",
                "download": {"status": "notfound"},
            }
        )
    module["candidates"].extend(
        [
            {
                "location": DEFAULT_SERVER_PATH + PATHS["pd_"],
                "source": "microsoft",
                "download": {"status": "notfound"},
            },
            {
                "location": DEFAULT_SERVER_PATH + PATHS["pdb"],
                "source": "microsoft",
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
            },
        ]
    )
    if not filtered:
        module["candidates"].append(
            {
                "location": DEFAULT_SERVER_PATH + PATHS["src"],
                "source": "microsoft",
                "download": {"status": "notfound"},
            }
        )

    return {
        "stacktraces": [stacktrace],
        "modules": [module],
        "status": "completed",
    }


SUCCESS_WINDOWS_FILTERED = _make_successful_result(filtered=True)
SUCCESS_WINDOWS = _make_successful_result()


def _make_error_result(
    frame_status,
    debug_status,
    download_error,
    source=(None, True),
    base_url=DEFAULT_SERVER_PATH,
):
    """
    Builds a standard error result. `download_error` should be an ObjectDownloadInfo. source is
    a tuple composed of the expected source name, and whether it is expected to have any candidates.
    """
    stacktrace = copy.deepcopy(STACKTRACE_RESULT)
    stacktrace["frames"][0].update({"status": frame_status})

    module = copy.deepcopy(MODULE_RESULT)
    module.update(
        {
            "debug_status": debug_status,
            "features": {
                "has_debug_info": False,
                "has_sources": False,
                "has_symbols": False,
                "has_unwind_info": False,
            },
            "arch": "unknown",
        }
    )
    (source_name, missing_candidates) = source if source else (None, True)
    base_candidate = {
        "download": download_error,
        "source": source_name,
    }
    if source_name is None:
        pass
    elif missing_candidates:
        module["candidates"] = [
            {"location": "No object files listed on this source", **base_candidate}
        ]
    else:
        module["candidates"] = [
            {"location": base_url + PATHS["dwarf"], **base_candidate},
            {"location": base_url + PATHS["pd_"], **base_candidate},
            {"location": base_url + PATHS["pdb"], **base_candidate},
            {"location": base_url + PATHS["src"], **base_candidate},
        ]

    return {
        "stacktraces": [stacktrace],
        "modules": [module],
        "status": "completed",
    }


@pytest.fixture(params=[True, False], ids=["cachedir", "no_cachedir"])
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
    for i in range(2):
        service = symbolicator(cache_dir=cache_dir_param)
        service.wait_healthcheck()

        response = service.post(f"/symbolicate?scope={scope}", json=input)
        response.raise_for_status()

        assert_symbolication(response.json(), SUCCESS_WINDOWS_FILTERED)

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
                cache_dir_param.join("symcaches")
                .join("5")
                .join(stored_in_scope)
                .listdir()
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
        assert_symbolication(response, SUCCESS_WINDOWS_FILTERED)

    assert set(hitcounter.hits) == {
        "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pd_",
        "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb",
    }

    for key, count in hitcounter.hits.items():
        assert count < 20, (key, count)


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

    assert_symbolication(responses[-1], SUCCESS_WINDOWS_FILTERED)
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
        source = ("broken", True)
        download_error = {"status": "notfound"}
        debug_status = "missing"
    elif statuscode == 500:
        source = ("broken", False)
        download_error = {
            "status": "error",
            "details": "download failed: 500 Internal Server Error",
        }
        debug_status = "fetching_failed"
    else:
        source = ("broken", False)
        download_error = {"status": "notfound"}
        debug_status = "missing"

    expected = _make_error_result(
        frame_status="missing",
        debug_status=debug_status,
        download_error=download_error,
        source=source,
        base_url=f"{hitcounter.url}/respond_statuscode/{statuscode}/",
    )
    assert_symbolication(
        response, expected, {"status code": statuscode, "bucket type": bucket_type}
    )


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
