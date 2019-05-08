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
                    "status": "missing",
                    "original_index": 0,
                    "package": "C:\\projects\\breakpad-tools\\windows\\Release\\crash.exe",
                    "instruction_addr": "0x2a2a3d",
                    "trust": "context",
                },
                {
                    "status": "missing",
                    "original_index": 1,
                    "package": "C:\\projects\\breakpad-tools\\windows\\Release\\crash.exe",
                    "instruction_addr": "0x2a28d0",
                    "trust": "fp",
                },
                {
                    "status": "symbolicated",
                    "original_index": 2,
                    "instruction_addr": "0x7584e9bf",
                    "package": "C:\\Windows\\System32\\rpcrt4.dll",
                    "symbol": "?FreeWrapper@@YGXPAX@Z",
                    "sym_addr": "0x7584e960",
                    "function": "FreeWrapper(void *)",
                    "lineno": 0,
                    "trust": "scan",
                },
                {
                    "status": "symbolicated",
                    "original_index": 4,
                    "instruction_addr": "0x70b7ae3f",
                    "package": "C:\\Windows\\System32\\dbgcore.dll",
                    "symbol": "?DetermineOutputProvider@@YGJPAVMiniDumpAllocationProvider@@PAXQAU_MINIDUMP_CALLBACK_INFORMATION@@PAPAVMiniDumpOutputProvider@@@Z",
                    "sym_addr": "0x70b7ad6b",
                    "function": "DetermineOutputProvider(class MiniDumpAllocationProvider *,void *,struct _MINIDUMP_CALLBACK_INFORMATION * const,class MiniDumpOutputProvider * *)",
                    "lineno": 0,
                    "trust": "scan",
                },
                {
                    "status": "symbolicated",
                    "original_index": 6,
                    "instruction_addr": "0x7584e9bf",
                    "package": "C:\\Windows\\System32\\rpcrt4.dll",
                    "symbol": "?FreeWrapper@@YGXPAX@Z",
                    "sym_addr": "0x7584e960",
                    "function": "FreeWrapper(void *)",
                    "lineno": 0,
                    "trust": "scan",
                },
                {
                    "status": "symbolicated",
                    "original_index": 9,
                    "instruction_addr": "0x750662c3",
                    "package": "C:\\Windows\\System32\\kernel32.dll",
                    "symbol": "@BaseThreadInitThunk@12",
                    "sym_addr": "0x750662a0",
                    "function": "@BaseThreadInitThunk@12",
                    "lineno": 0,
                    "trust": "fp",
                },
                {
                    "status": "symbolicated",
                    "original_index": 10,
                    "instruction_addr": "0x771d0f78",
                    "package": "C:\\Windows\\System32\\ntdll.dll",
                    "symbol": "__RtlUserThreadStart@8",
                    "sym_addr": "0x771d0f4a",
                    "function": "__RtlUserThreadStart@8",
                    "lineno": 0,
                    "trust": "cfi",
                },
                {
                    "status": "symbolicated",
                    "original_index": 11,
                    "instruction_addr": "0x771d0f43",
                    "package": "C:\\Windows\\System32\\ntdll.dll",
                    "symbol": "_RtlUserThreadStart@8",
                    "sym_addr": "0x771d0f29",
                    "function": "_RtlUserThreadStart@8",
                    "lineno": 0,
                    "trust": "cfi",
                },
            ],
            "is_requesting": True,
            "registers": {
                "eax": "0x0",
                "ebp": "0x10ff670",
                "ebx": "0xfe5000",
                "ecx": "0x10ff670",
                "edi": "0x13bfd78",
                "edx": "0x7",
                "eflags": "0x10246",
                "eip": "0x2a2a3d",
                "esi": "0x759c6314",
                "esp": "0x10ff644",
            },
            "thread_id": 1636,
        },
        {
            "frames": [
                {
                    "status": "symbolicated",
                    "original_index": 0,
                    "instruction_addr": "0x771e016c",
                    "package": "C:\\Windows\\System32\\ntdll.dll",
                    "symbol": "ZwWaitForWorkViaWorkerFactory@20",
                    "sym_addr": "0x771e0160",
                    "function": "ZwWaitForWorkViaWorkerFactory@20",
                    "lineno": 0,
                    "trust": "context",
                },
                {
                    "status": "symbolicated",
                    "original_index": 1,
                    "instruction_addr": "0x771a6a10",
                    "package": "C:\\Windows\\System32\\ntdll.dll",
                    "symbol": "TppWorkerThread@4",
                    "sym_addr": "0x771a6770",
                    "function": "TppWorkerThread@4",
                    "lineno": 0,
                    "trust": "cfi",
                },
                {
                    "status": "symbolicated",
                    "original_index": 2,
                    "instruction_addr": "0x750662c3",
                    "package": "C:\\Windows\\System32\\kernel32.dll",
                    "symbol": "@BaseThreadInitThunk@12",
                    "sym_addr": "0x750662a0",
                    "function": "@BaseThreadInitThunk@12",
                    "lineno": 0,
                    "trust": "cfi",
                },
                {
                    "status": "symbolicated",
                    "original_index": 3,
                    "instruction_addr": "0x771d0f78",
                    "package": "C:\\Windows\\System32\\ntdll.dll",
                    "symbol": "__RtlUserThreadStart@8",
                    "sym_addr": "0x771d0f4a",
                    "function": "__RtlUserThreadStart@8",
                    "lineno": 0,
                    "trust": "cfi",
                },
                {
                    "status": "symbolicated",
                    "original_index": 4,
                    "instruction_addr": "0x771d0f43",
                    "package": "C:\\Windows\\System32\\ntdll.dll",
                    "symbol": "_RtlUserThreadStart@8",
                    "sym_addr": "0x771d0f29",
                    "function": "_RtlUserThreadStart@8",
                    "lineno": 0,
                    "trust": "cfi",
                },
            ],
            "is_requesting": False,
            "registers": {
                "eax": "0x0",
                "ebp": "0x159faa4",
                "ebx": "0x13b0990",
                "ecx": "0x0",
                "edi": "0x13b4af0",
                "edx": "0x0",
                "eflags": "0x216",
                "eip": "0x771e016c",
                "esi": "0x13b4930",
                "esp": "0x159f900",
            },
            "thread_id": 3580,
        },
        {
            "frames": [
                {
                    "status": "symbolicated",
                    "original_index": 0,
                    "instruction_addr": "0x771e016c",
                    "package": "C:\\Windows\\System32\\ntdll.dll",
                    "symbol": "ZwWaitForWorkViaWorkerFactory@20",
                    "sym_addr": "0x771e0160",
                    "function": "ZwWaitForWorkViaWorkerFactory@20",
                    "lineno": 0,
                    "trust": "context",
                },
                {
                    "status": "symbolicated",
                    "original_index": 1,
                    "instruction_addr": "0x771a6a10",
                    "package": "C:\\Windows\\System32\\ntdll.dll",
                    "symbol": "TppWorkerThread@4",
                    "sym_addr": "0x771a6770",
                    "function": "TppWorkerThread@4",
                    "lineno": 0,
                    "trust": "cfi",
                },
                {
                    "status": "symbolicated",
                    "original_index": 2,
                    "instruction_addr": "0x750662c3",
                    "package": "C:\\Windows\\System32\\kernel32.dll",
                    "symbol": "@BaseThreadInitThunk@12",
                    "sym_addr": "0x750662a0",
                    "function": "@BaseThreadInitThunk@12",
                    "lineno": 0,
                    "trust": "cfi",
                },
                {
                    "status": "symbolicated",
                    "original_index": 3,
                    "instruction_addr": "0x771d0f78",
                    "package": "C:\\Windows\\System32\\ntdll.dll",
                    "symbol": "__RtlUserThreadStart@8",
                    "sym_addr": "0x771d0f4a",
                    "function": "__RtlUserThreadStart@8",
                    "lineno": 0,
                    "trust": "cfi",
                },
                {
                    "status": "symbolicated",
                    "original_index": 4,
                    "instruction_addr": "0x771d0f43",
                    "package": "C:\\Windows\\System32\\ntdll.dll",
                    "symbol": "_RtlUserThreadStart@8",
                    "sym_addr": "0x771d0f29",
                    "function": "_RtlUserThreadStart@8",
                    "lineno": 0,
                    "trust": "cfi",
                },
            ],
            "is_requesting": False,
            "registers": {
                "eax": "0x0",
                "ebp": "0x169fb98",
                "ebx": "0x13b0990",
                "ecx": "0x0",
                "edi": "0x13b7c28",
                "edx": "0x0",
                "eflags": "0x202",
                "eip": "0x771e016c",
                "esi": "0x13b7a68",
                "esp": "0x169f9f4",
            },
            "thread_id": 2600,
        },
        {
            "frames": [
                {
                    "status": "symbolicated",
                    "original_index": 0,
                    "instruction_addr": "0x771df3dc",
                    "package": "C:\\Windows\\System32\\ntdll.dll",
                    "symbol": "ZwGetContextThread@8",
                    "sym_addr": "0x771df3d0",
                    "function": "ZwGetContextThread@8",
                    "lineno": 0,
                    "trust": "context",
                },
                {
                    "status": "symbolicated",
                    "original_index": 1,
                    "instruction_addr": "0x76e75dbf",
                    "package": "C:\\Windows\\System32\\KERNELBASE.dll",
                    "symbol": "NlsIsUserDefaultLocale@4",
                    "sym_addr": "0x76e75d90",
                    "function": "NlsIsUserDefaultLocale@4",
                    "lineno": 0,
                    "trust": "cfi",
                },
            ],
            "is_requesting": False,
            "registers": {
                "eax": "0x0",
                "ebp": "0x179f2b8",
                "ebx": "0x17b1aa0",
                "ecx": "0x0",
                "edi": "0x17b1a90",
                "edx": "0x0",
                "eflags": "0x206",
                "eip": "0x771df3dc",
                "esi": "0x2cc",
                "esp": "0x179f2ac",
            },
            "thread_id": 2920,
        },
    ],
    "modules": [
        {
            "debug_status": "missing",
            "unwind_status": "missing",
            "arch": "unknown",
            "type": "pe",
            "code_id": "5ab380779000",
            "code_file": "C:\\projects\\breakpad-tools\\windows\\Release\\crash.exe",
            "debug_id": "3249d99d-0c40-4931-8610-f4e4fb0b6936-1",
            "debug_file": "C:\\projects\\breakpad-tools\\windows\\Release\\crash.pdb",
            "image_addr": "0x2a0000",
            "image_size": 36864,
        },
        {
            "arch": "x86",
            "code_file": "C:\\Windows\\System32\\dbghelp.dll",
            "code_id": "57898e12145000",
            "debug_file": "dbghelp.pdb",
            "debug_id": "9c2a902b-6fdf-40ad-8308-588a41d572a0-1",
            "image_addr": "0x70850000",
            "image_size": 1_331_200,
            "debug_status": "found",
            "unwind_status": "unused",
            "type": "pe",
        },
        {
            "arch": "unknown",
            "code_file": "C:\\Windows\\System32\\msvcp140.dll",
            "code_id": "589abc846c000",
            "debug_file": "msvcp140.i386.pdb",
            "debug_id": "bf5257f7-8c26-43dd-9bb7-901625e1136a-1",
            "image_addr": "0x709a0000",
            "image_size": 442_368,
            "debug_status": "unused",
            "unwind_status": "unused",
            "type": "pe",
        },
        {
            "arch": "unknown",
            "code_file": "C:\\Windows\\System32\\apphelp.dll",
            "code_id": "57898eeb92000",
            "debug_file": "apphelp.pdb",
            "debug_id": "8daf7773-372f-460a-af38-944e193f7e33-1",
            "image_addr": "0x70a10000",
            "image_size": 598_016,
            "debug_status": "unused",
            "unwind_status": "unused",
            "type": "pe",
        },
        {
            "debug_status": "found",
            "unwind_status": "found",
            "arch": "x86",
            "type": "pe",
            "code_id": "57898dab25000",
            "code_file": "C:\\Windows\\System32\\dbgcore.dll",
            "debug_id": "aec7ef2f-df4b-4642-a471-4c3e5fe8760a-1",
            "debug_file": "dbgcore.pdb",
            "image_addr": "0x70b70000",
            "image_size": 151_552,
        },
        {
            "arch": "unknown",
            "code_file": "C:\\Windows\\System32\\VCRUNTIME140.dll",
            "code_id": "589abc7714000",
            "debug_file": "vcruntime140.i386.pdb",
            "debug_id": "0ed80a50-ecda-472b-86a4-eb6c833f8e1b-1",
            "image_addr": "0x70c60000",
            "image_size": 81920,
            "debug_status": "unused",
            "unwind_status": "unused",
            "type": "pe",
        },
        {
            "arch": "unknown",
            "code_file": "C:\\Windows\\System32\\CRYPTBASE.dll",
            "code_id": "57899141a000",
            "debug_file": "cryptbase.pdb",
            "debug_id": "147c51fb-7ca1-408f-85b5-285f2ad6f9c5-1",
            "image_addr": "0x73ba0000",
            "image_size": 40960,
            "debug_status": "unused",
            "unwind_status": "unused",
            "type": "pe",
        },
        {
            "arch": "unknown",
            "code_file": "C:\\Windows\\System32\\sspicli.dll",
            "code_id": "59bf30e31f000",
            "debug_file": "wsspicli.pdb",
            "debug_id": "51e432b1-0450-4b19-8ed1-6d4335f9f543-1",
            "image_addr": "0x73bb0000",
            "image_size": 126_976,
            "debug_status": "unused",
            "unwind_status": "unused",
            "type": "pe",
        },
        {
            "arch": "unknown",
            "code_file": "C:\\Windows\\System32\\advapi32.dll",
            "code_id": "5a49bb7677000",
            "debug_file": "advapi32.pdb",
            "debug_id": "0c799483-b549-417d-8433-4331852031fe-1",
            "image_addr": "0x73c70000",
            "image_size": 487_424,
            "debug_status": "unused",
            "unwind_status": "unused",
            "type": "pe",
        },
        {
            "arch": "unknown",
            "code_file": "C:\\Windows\\System32\\msvcrt.dll",
            "code_id": "57899155be000",
            "debug_file": "msvcrt.pdb",
            "debug_id": "6f6409b3-d520-43c7-9b2f-62e00bfe761c-1",
            "image_addr": "0x73cf0000",
            "image_size": 778_240,
            "debug_status": "unused",
            "unwind_status": "unused",
            "type": "pe",
        },
        {
            "arch": "unknown",
            "code_file": "C:\\Windows\\System32\\sechost.dll",
            "code_id": "598942c741000",
            "debug_file": "sechost.pdb",
            "debug_id": "6f6a05dd-0a80-478b-a419-9b88703bf75b-1",
            "image_addr": "0x74450000",
            "image_size": 266_240,
            "debug_status": "unused",
            "unwind_status": "unused",
            "type": "pe",
        },
        {
            "debug_status": "found",
            "unwind_status": "found",
            "arch": "x86",
            "type": "pe",
            "code_id": "590285e9e0000",
            "code_file": "C:\\Windows\\System32\\kernel32.dll",
            "debug_id": "d3474559-96f7-47d6-bf43-c176b2171e68-1",
            "debug_file": "wkernel32.pdb",
            "image_addr": "0x75050000",
            "image_size": 917_504,
            "type": "pe",
        },
        {
            "arch": "unknown",
            "code_file": "C:\\Windows\\System32\\bcryptPrimitives.dll",
            "code_id": "59b0df8f5a000",
            "debug_file": "bcryptprimitives.pdb",
            "debug_id": "287b19c3-9209-4a2b-bb8f-bcc37f411b11-1",
            "image_addr": "0x75130000",
            "image_size": 368_640,
            "debug_status": "unused",
            "unwind_status": "unused",
            "type": "pe",
        },
        {
            "debug_status": "found",
            "unwind_status": "found",
            "arch": "x86",
            "type": "pe",
            "code_id": "5a49bb75c1000",
            "code_file": "C:\\Windows\\System32\\rpcrt4.dll",
            "debug_id": "ae131c67-27a7-4fa1-9916-b5a4aef41190-1",
            "debug_file": "wrpcrt4.pdb",
            "image_addr": "0x75810000",
            "image_size": 790_528,
        },
        {
            "arch": "unknown",
            "code_file": "C:\\Windows\\System32\\ucrtbase.dll",
            "code_id": "59bf2b5ae0000",
            "debug_file": "ucrtbase.pdb",
            "debug_id": "6bedcbce-0a3a-40e9-8040-81c2c8c6cc2f-1",
            "image_addr": "0x758f0000",
            "image_size": 917_504,
            "debug_status": "unused",
            "unwind_status": "unused",
            "type": "pe",
        },
        {
            "arch": "x86",
            "code_file": "C:\\Windows\\System32\\KERNELBASE.dll",
            "code_id": "59bf2bcf1a1000",
            "debug_file": "wkernelbase.pdb",
            "debug_id": "8462294a-c645-402d-ac82-a4e95f61ddf9-1",
            "image_addr": "0x76db0000",
            "image_size": 1_708_032,
            "debug_status": "found",
            "unwind_status": "unused",
            "type": "pe",
        },
        {
            "debug_status": "found",
            "unwind_status": "found",
            "arch": "x86",
            "type": "pe",
            "code_id": "59b0d8f3183000",
            "code_file": "C:\\Windows\\System32\\ntdll.dll",
            "debug_id": "971f98e5-ce60-41ff-b2d7-235bbeb34578-1",
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
                            "layout": {"type": "symstore"},
                            "filters": {"filetypes": ["pdb", "pe"]},
                            "url": f"{hitcounter.url}/msdl/",
                            "is_public": True,
                        }
                    ]
                )
            },
        )
        response.raise_for_status()

    assert response.json() == MINIDUMP_SUCCESS
