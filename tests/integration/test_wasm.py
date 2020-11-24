import pytest


WASM_DATA = {
    "stacktraces": [
        {
            "registers": {},
            "frames": [
                {
                    "instruction_addr": "0x8c",
                    "in_module": 0,
                }
            ],
        },
    ],
    "modules": [
        {
            "type": "wasm",
            "debug_id": "bda18fd8-5d4a-4eb8-9302-2d6bfad846b1",
            "code_id": "bda18fd85d4a4eb893022d6bfad846b1",
            "debug_file": "file://foo.invalid/demo.wasm",
        }
    ],
}

SUCCESS_WASM = {
    "modules": [
        {
            "arch": "wasm",
            "code_id": "bda18fd85d4a4eb893022d6bfad846b1",
            "debug_file": "file://foo.invalid/demo.wasm",
            "debug_id": "bda18fd8-5d4a-4eb8-9302-2d6bfad846b1",
            "debug_status": "found",
            "features": {
                "has_debug_info": True,
                "has_sources": False,
                "has_symbols": True,
                "has_unwind_info": False
            },
            "image_addr": "0x0",
            "type": "wasm"
        }
    ],
    "stacktraces": [
        {
            "frames": [
                {
                    "abs_path": "/Users/mitsuhiko/Development/wasm-example/simple/src/lib.rs",
                    "filename": "src/lib.rs",
                    "function": "internal_func",
                    "instruction_addr": "0x8c",
                    "lang": "rust",
                    "lineno": 4,
                    "original_index": 0,
                    "status": "symbolicated",
                    "sym_addr": "0x8b",
                    "symbol": "internal_func"
                }
            ]
        }
    ],
    "status": "completed"
}


def test_basic_wasm(symbolicator, hitcounter):
    scope = "myscope"

    import json
    input = dict(
        **WASM_DATA,
        sources=[
            {
                "type": "http",
                "id": "stuff",
                "layout": {"type": "native"},
                "filters": {"filetypes": ["wasm_debug"]},
                "url": f"{hitcounter.url}/symbols/",
                "is_public": False,
            }
        ]
    )

    service = symbolicator()
    service.wait_healthcheck()

    response = service.post(f"/symbolicate?scope={scope}", json=input)
    response.raise_for_status()

    assert response.json() == SUCCESS_WASM
