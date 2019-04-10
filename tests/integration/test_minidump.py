import json

MINIDUMP_SUCCESS = {
    "status": "completed",
    "system_info": {
        "os_name": "Windows NT",
        "os_version": "10.0.14393",
        "os_build": "",
        "cpu_arch": "x86",
    },
    "crashed": True,
    "crash_reason": "EXCEPTION_ACCESS_VIOLATION_WRITE",
    "assertion": "",
    "stacktraces": [
        {
            "frames": [
                {
                    "status": "missing_debug_file",
                    "original_index": 0,
                    "instruction_addr": "0x2a2a3d",
                },
                {
                    "status": "missing_debug_file",
                    "original_index": 1,
                    "instruction_addr": "0x2a28d0",
                },
                {
                    "status": "symbolicated",
                    "original_index": 2,
                    "instruction_addr": "0x7584e9bf",
                    "package": "rpcrt4.dll",
                    "symbol": "?FreeWrapper@@YGXPAX@Z",
                    "sym_addr": "0x7584e960",
                    "function": "FreeWrapper(void *)",
                    "lineno": 0,
                },
                {
                    "status": "unknown_image",
                    "original_index": 3,
                    "instruction_addr": "0x70850000",
                },
                {
                    "status": "symbolicated",
                    "original_index": 4,
                    "instruction_addr": "0x70b7ae3f",
                    "package": "dbgcore.dll",
                    "symbol": "?DetermineOutputProvider@@YGJPAVMiniDumpAllocationProvider@@PAXQAU_MINIDUMP_CALLBACK_INFORMATION@@PAPAVMiniDumpOutputProvider@@@Z",
                    "sym_addr": "0x70b7ad6b",
                    "function": "DetermineOutputProvider(class MiniDumpAllocationProvider *,void *,struct _MINIDUMP_CALLBACK_INFORMATION * const,class MiniDumpOutputProvider * *)",
                    "lineno": 0,
                },
                {
                    "status": "missing_symbol",
                    "original_index": 5,
                    "instruction_addr": "0x75810000",
                },
                {
                    "status": "symbolicated",
                    "original_index": 6,
                    "instruction_addr": "0x7584e9bf",
                    "package": "rpcrt4.dll",
                    "symbol": "?FreeWrapper@@YGXPAX@Z",
                    "sym_addr": "0x7584e960",
                    "function": "FreeWrapper(void *)",
                    "lineno": 0,
                },
                {
                    "status": "missing_debug_file",
                    "original_index": 7,
                    "instruction_addr": "0x2a3435",
                },
                {
                    "status": "missing_debug_file",
                    "original_index": 8,
                    "instruction_addr": "0x2a2d97",
                },
                {
                    "status": "symbolicated",
                    "original_index": 9,
                    "instruction_addr": "0x750662c3",
                    "package": "kernel32.dll",
                    "symbol": "@BaseThreadInitThunk@12",
                    "sym_addr": "0x750662a0",
                    "function": "@BaseThreadInitThunk@12",
                    "lineno": 0,
                },
                {
                    "status": "symbolicated",
                    "original_index": 10,
                    "instruction_addr": "0x771d0f78",
                    "package": "ntdll.dll",
                    "symbol": "__RtlUserThreadStart@8",
                    "sym_addr": "0x771d0f4a",
                    "function": "__RtlUserThreadStart@8",
                    "lineno": 0,
                },
                {
                    "status": "symbolicated",
                    "original_index": 11,
                    "instruction_addr": "0x771d0f43",
                    "package": "ntdll.dll",
                    "symbol": "_RtlUserThreadStart@8",
                    "sym_addr": "0x771d0f29",
                    "function": "_RtlUserThreadStart@8",
                    "lineno": 0,
                },
            ]
        },
        {
            "frames": [
                {
                    "status": "symbolicated",
                    "original_index": 0,
                    "instruction_addr": "0x771e016c",
                    "package": "ntdll.dll",
                    "symbol": "ZwWaitForWorkViaWorkerFactory@20",
                    "sym_addr": "0x771e0160",
                    "function": "ZwWaitForWorkViaWorkerFactory@20",
                    "lineno": 0,
                },
                {
                    "status": "symbolicated",
                    "original_index": 1,
                    "instruction_addr": "0x771a6a10",
                    "package": "ntdll.dll",
                    "symbol": "TppWorkerThread@4",
                    "sym_addr": "0x771a6770",
                    "function": "TppWorkerThread@4",
                    "lineno": 0,
                },
                {
                    "status": "symbolicated",
                    "original_index": 2,
                    "instruction_addr": "0x750662c3",
                    "package": "kernel32.dll",
                    "symbol": "@BaseThreadInitThunk@12",
                    "sym_addr": "0x750662a0",
                    "function": "@BaseThreadInitThunk@12",
                    "lineno": 0,
                },
                {
                    "status": "symbolicated",
                    "original_index": 3,
                    "instruction_addr": "0x771d0f78",
                    "package": "ntdll.dll",
                    "symbol": "__RtlUserThreadStart@8",
                    "sym_addr": "0x771d0f4a",
                    "function": "__RtlUserThreadStart@8",
                    "lineno": 0,
                },
                {
                    "status": "symbolicated",
                    "original_index": 4,
                    "instruction_addr": "0x771d0f43",
                    "package": "ntdll.dll",
                    "symbol": "_RtlUserThreadStart@8",
                    "sym_addr": "0x771d0f29",
                    "function": "_RtlUserThreadStart@8",
                    "lineno": 0,
                },
            ]
        },
        {
            "frames": [
                {
                    "status": "symbolicated",
                    "original_index": 0,
                    "instruction_addr": "0x771e016c",
                    "package": "ntdll.dll",
                    "symbol": "ZwWaitForWorkViaWorkerFactory@20",
                    "sym_addr": "0x771e0160",
                    "function": "ZwWaitForWorkViaWorkerFactory@20",
                    "lineno": 0,
                },
                {
                    "status": "symbolicated",
                    "original_index": 1,
                    "instruction_addr": "0x771a6a10",
                    "package": "ntdll.dll",
                    "symbol": "TppWorkerThread@4",
                    "sym_addr": "0x771a6770",
                    "function": "TppWorkerThread@4",
                    "lineno": 0,
                },
                {
                    "status": "symbolicated",
                    "original_index": 2,
                    "instruction_addr": "0x750662c3",
                    "package": "kernel32.dll",
                    "symbol": "@BaseThreadInitThunk@12",
                    "sym_addr": "0x750662a0",
                    "function": "@BaseThreadInitThunk@12",
                    "lineno": 0,
                },
                {
                    "status": "symbolicated",
                    "original_index": 3,
                    "instruction_addr": "0x771d0f78",
                    "package": "ntdll.dll",
                    "symbol": "__RtlUserThreadStart@8",
                    "sym_addr": "0x771d0f4a",
                    "function": "__RtlUserThreadStart@8",
                    "lineno": 0,
                },
                {
                    "status": "symbolicated",
                    "original_index": 4,
                    "instruction_addr": "0x771d0f43",
                    "package": "ntdll.dll",
                    "symbol": "_RtlUserThreadStart@8",
                    "sym_addr": "0x771d0f29",
                    "function": "_RtlUserThreadStart@8",
                    "lineno": 0,
                },
            ]
        },
        {
            "frames": [
                {
                    "status": "symbolicated",
                    "original_index": 0,
                    "instruction_addr": "0x771df3dc",
                    "package": "ntdll.dll",
                    "symbol": "ZwGetContextThread@8",
                    "sym_addr": "0x771df3d0",
                    "function": "ZwGetContextThread@8",
                    "lineno": 0,
                },
                {
                    "status": "unknown_image",
                    "original_index": 1,
                    "instruction_addr": "0x76e75dc0",
                },
            ]
        },
    ],
    "modules": [
        {
            "status": "missing_debug_file",
            "arch": "unknown",
            "type": "pe",
            "code_id": "5AB380779000",
            "code_file": "crash.exe",
            "debug_id": "3249D99D0C4049318610F4E4FB0B69361",
            "debug_file": "crash.pdb",
            "image_addr": "0x2a0000",
            "image_size": 36864,
        },
        {
            "status": "found",
            "arch": "x86",
            "type": "pe",
            "code_id": "57898DAB25000",
            "code_file": "dbgcore.dll",
            "debug_id": "AEC7EF2FDF4B4642A4714C3E5FE8760A1",
            "debug_file": "dbgcore.pdb",
            "image_addr": "0x70b70000",
            "image_size": 151_552,
        },
        {
            "status": "found",
            "arch": "x86",
            "type": "pe",
            "code_id": "590285E9e0000",
            "code_file": "kernel32.dll",
            "debug_id": "D347455996F747D6BF43C176B2171E681",
            "debug_file": "wkernel32.pdb",
            "image_addr": "0x75050000",
            "image_size": 917_504,
        },
        {
            "status": "found",
            "arch": "x86",
            "type": "pe",
            "code_id": "5A49BB75c1000",
            "code_file": "rpcrt4.dll",
            "debug_id": "AE131C6727A74FA19916B5A4AEF411901",
            "debug_file": "wrpcrt4.pdb",
            "image_addr": "0x75810000",
            "image_size": 790_528,
        },
        {
            "status": "found",
            "arch": "x86",
            "type": "pe",
            "code_id": "59B0D8F3183000",
            "code_file": "ntdll.dll",
            "debug_id": "971F98E5CE6041FFB2D7235BBEB345781",
            "debug_file": "wntdll.pdb",
            "image_addr": "0x77170000",
            "image_size": 1_585_152,
        },
    ],
}


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

    assert response.json() == MINIDUMP_SUCCESS
