import copy
import time

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
