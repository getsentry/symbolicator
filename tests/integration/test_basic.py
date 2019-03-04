def test_basic(symbolicator):
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
            }
        ],
    }

    service = symbolicator()
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
