import uuid


debug_id = uuid.UUID("502fc0a5-1ec1-3e47-9998-684fa139dca7")


MACHO_HELLO_DATA = {
    "modules": [
        {
            "type": "macho",
            "code_id": str(debug_id).upper(),
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
            "code_file": None,
            "code_id": str(debug_id).upper(),
            "debug_file": None,
            "debug_id": str(debug_id),
            "image_addr": "0x100000000",
            "image_size": 4096,
            "status": "found",
            "type": "macho",
        }
    ],
    "signal": None,
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
                    "package": None,
                    "status": "symbolicated",
                    "sym_addr": "0x100000fa0",
                    "symbol": "main",
                }
            ]
        }
    ],
    "status": "completed",
}


def test_s3(symbolicator, hitcounter, s3_bucket_config, s3):
    uuid = debug_id.hex
    s3.meta.client.put_object(
        Body=open("tests/fixtures/hello.dsym", "rb"),
        Bucket=s3_bucket_config["bucket"],
        Key=f"_.dwarf/mach-uuid-sym-{uuid}/_.dwarf",
    )

    input = dict(
        sources=[dict(id="s3", layout="symstore", **s3_bucket_config)],
        **MACHO_HELLO_DATA,
    )

    service = symbolicator()
    service.wait_healthcheck()

    response = service.post("/symbolicate", json=input)
    response.raise_for_status()
    response = response.json()
    assert response == MACHO_SUCCESS
