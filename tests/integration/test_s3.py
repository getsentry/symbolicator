import copy
import uuid
import pytest


debug_id = uuid.UUID("502fc0a5-1ec1-3e47-9998-684fa139dca7")


MACHO_HELLO_DATA = {
    "modules": [
        {
            "type": "macho",
            "code_id": str(debug_id).replace("-", "").lower(),
            "debug_id": str(debug_id),
            "image_vmaddr": "0x0000000100000000",
            "image_size": 4096,
            "image_addr": "0x0000000100000000",
            "code_name": "Foo.app/Contents/Foo",
        }
    ],
    "stacktraces": [
        {
            "registers": {},
            "frames": [
                {"function": "unknown", "instruction_addr": "0x0000000100000fa0"}
            ],
        }
    ],
}

MACHO_SUCCESS = {
    "modules": [
        {
            "arch": "x86_64",
            "code_id": str(debug_id).replace("-", "").lower(),
            "debug_id": str(debug_id),
            "image_addr": "0x100000000",
            "image_size": 4096,
            "debug_status": "found",
            "features": {
                "has_debug_info": True,
                "has_sources": False,
                "has_symbols": True,
                "has_unwind_info": False,
            },
            "type": "macho",
            "candidates": [
                {
                    "download": {
                        "features": {
                            "has_debug_info": True,
                            "has_sources": False,
                            "has_symbols": True,
                            "has_unwind_info": False,
                        },
                        "status": "ok",
                    },
                    "location": "_.dwarf/mach-uuid-sym-502fc0a51ec13e479998684fa139dca7/_.dwarf",
                    "source": "s3",
                },
            ],
        }
    ],
    "stacktraces": [
        {
            "frames": [
                {
                    "abs_path": "/tmp/hello.c",
                    "filename": "hello.c",
                    "function": "main",
                    "instruction_addr": "0x100000fa0",
                    "lang": "c",
                    "lineno": 1,
                    "original_index": 0,
                    "status": "symbolicated",
                    "sym_addr": "0x100000fa0",
                    "symbol": "main",
                }
            ]
        }
    ],
    "status": "completed",
}


@pytest.mark.parametrize("casing", ["default", "lowercase", "uppercase"])
def test_s3(symbolicator, hitcounter, s3_bucket_config, s3, casing):
    uuid = debug_id.hex
    key = f"_.dwarf/mach-uuid-sym-{uuid}/_.dwarf"
    if casing == "lowercase":
        key = key.lower()
    elif casing == "uppercase":
        key = key.upper()

    s3.meta.client.put_object(
        Body=open("tests/fixtures/symbols/502F/C0A5/1EC1/3E47/9998/684FA139DCA7", "rb"),
        Bucket=s3_bucket_config["bucket"],
        Key=key,
    )

    input = dict(
        sources=[
            dict(
                id="s3",
                layout={"type": "symstore", "casing": casing},
                **s3_bucket_config,
            )
        ],
        options={
            "dif_candidates": True,
        },
        **MACHO_HELLO_DATA,
    )

    service = symbolicator()
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()
    response = response.json()
    if casing == "uppercase":
        success = copy.deepcopy(MACHO_SUCCESS)
        location = success["modules"][0]["candidates"][0]["location"]
        success["modules"][0]["candidates"][0]["location"] = location.upper()
    else:
        assert response == MACHO_SUCCESS
