import json


def test_basic(symbolicator, hitcounter):
    service = symbolicator()
    service.wait_healthcheck()

    with open("tests/fixtures/windows.dmp", "rb") as f:
        response = service.post(
            "/minidump",
            files={"upload_file_minidump": f},
            data={
                "sources": json.dumps(
                    [
                        {
                            "type": "http",
                            "id": "microsoft",
                            "layout": "symstore",
                            "filetypes": ["pdb", "pe"],
                            "url": f"{hitcounter.url}/msdl/",
                            "is_public": True,
                        }
                    ]
                )
            },
        )
        response.raise_for_status()

        assert response.json == {}
