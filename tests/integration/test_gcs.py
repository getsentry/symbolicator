import uuid
import pytest


debug_id_libdyld = uuid.UUID("e514c946-4eed-3be5-943a-2c61d9241fad")
debug_id_cf = uuid.UUID("719044f9-5fe2-3ee0-ab14-504def42b100")


MACHO_TEST_DATA = {
    "modules": [
        {
            "type": "macho",
            "code_id": str(debug_id_libdyld).replace("-", "").lower(),
            "debug_id": str(debug_id_libdyld),
            "image_vmaddr": "0x18052d000",
            "image_size": 20480,
            "image_addr": "0x190605000",
            "code_name": "/usr/lib/system/libdyld.dylib",
        },
        {
            "type": "macho",
            "code_id": str(debug_id_cf).replace("-", "").lower(),
            "debug_id": str(debug_id_cf),
            "image_vmaddr": "0x18151a000",
            "image_size": 3678208,
            "image_addr": "0x1915f2000",
            "code_name": "/usr/lib/system/libdyld.dylib",
        },
    ],
    "stacktraces": [
        {
            "registers": {},
            "frames": [
                {"function": "<redacted>", "instruction_addr": "0x19060959c"},
                {"function": "<redacted>", "instruction_addr": "0x1916ca9a8"},
                {"function": "<redacted>", "instruction_addr": "0x1916ccb80"},
                {"function": "<redacted>", "instruction_addr": "0x1916cd42c"},
            ],
        }
    ],
}

MACHO_SUCCESS = {
    "modules": [
        {
            "type": "macho",
            "arch": "arm64",
            "code_id": str(debug_id_libdyld).replace("-", "").lower(),
            "debug_id": str(debug_id_libdyld),
            "image_size": 20480,
            "image_addr": "0x190605000",
            "debug_status": "found",
        },
        {
            "type": "macho",
            "arch": "arm64",
            "code_id": str(debug_id_cf).replace("-", "").lower(),
            "debug_id": str(debug_id_cf),
            "image_size": 3678208,
            "image_addr": "0x1915f2000",
            "debug_status": "found",
        },
    ],
    "stacktraces": [
        {
            "frames": [
                {
                    "function": "start",
                    "instruction_addr": "0x19060959c",
                    "lineno": 0,
                    "original_index": 0,
                    "status": "symbolicated",
                    "sym_addr": "0x190609598",
                    "symbol": "start",
                },
                {
                    "function": "__CFRunLoopRun",
                    "instruction_addr": "0x1916ca9a4",
                    "lineno": 0,
                    "original_index": 1,
                    "status": "symbolicated",
                    "sym_addr": "0x1916ca6c0",
                    "symbol": "__CFRunLoopRun",
                },
                {
                    "function": "__CFRunLoopDoBlocks",
                    "instruction_addr": "0x1916ccb7c",
                    "lineno": 0,
                    "original_index": 2,
                    "status": "symbolicated",
                    "sym_addr": "0x1916cca08",
                    "symbol": "__CFRunLoopDoBlocks",
                },
                {
                    "function": "__CFRUNLOOP_IS_CALLING_OUT_TO_A_SOURCE0_PERFORM_FUNCTION__",
                    "instruction_addr": "0x1916cd428",
                    "lineno": 0,
                    "original_index": 3,
                    "status": "symbolicated",
                    "sym_addr": "0x1916cd414",
                    "symbol": "__CFRUNLOOP_IS_CALLING_OUT_TO_A_SOURCE0_PERFORM_FUNCTION__",
                },
            ]
        }
    ],
    "status": "completed",
}


def test_gcs(symbolicator, hitcounter, ios_bucket_config):
    input = dict(sources=[ios_bucket_config], **MACHO_TEST_DATA)

    service = symbolicator()
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()
    response = response.json()

    assert response == MACHO_SUCCESS
