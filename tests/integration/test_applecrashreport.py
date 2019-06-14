import json

APPLE_CRASH_REPORT_SUCCESS = {
    "crash_reason": "objc_msgSend() selector name: respondsToSelector:\n"
    "  more information here",
    "crashed": True,
    "modules": [
        {
            "arch": "unknown",
            "code_file": "/Users/bruno/Documents/Unreal "
            "Projects/YetAnotherMac/MacNoEditor/YetAnotherMac.app/Contents/MacOS/YetAnotherMac",
            "code_id": "2d903291397d3d14bfca52c7fb8c5e00",
            "debug_file": "/Users/bruno/Documents/Unreal "
            "Projects/YetAnotherMac/MacNoEditor/YetAnotherMac.app/Contents/MacOS/YetAnotherMac",
            "debug_id": "2d903291-397d-3d14-bfca-52c7fb8c5e00",
            "debug_status": "missing",
            "image_addr": "0x10864e000",
            "image_size": 108797951,
            "type": "macho",
        },
        {
            "arch": "unknown",
            "code_file": "/Users/bruno/Documents/Unreal "
            "Projects/YetAnotherMac/MacNoEditor/YetAnotherMac.app/Contents/UE4/Engine/Binaries/ThirdParty/PhysX3/Mac/libPhysX3PROFILE.dylib",
            "code_id": "6deccee4a0523ea4bb67957b06f53ad1",
            "debug_file": "/Users/bruno/Documents/Unreal "
            "Projects/YetAnotherMac/MacNoEditor/YetAnotherMac.app/Contents/UE4/Engine/Binaries/ThirdParty/PhysX3/Mac/libPhysX3PROFILE.dylib",
            "debug_id": "6deccee4-a052-3ea4-bb67-957b06f53ad1",
            "debug_status": "unused",
            "image_addr": "0x112bb2000",
            "image_size": 2170879,
            "type": "macho",
        },
        {
            "arch": "unknown",
            "code_file": "/Users/bruno/Documents/Unreal "
            "Projects/YetAnotherMac/MacNoEditor/YetAnotherMac.app/Contents/UE4/Engine/Binaries/ThirdParty/PhysX3/Mac/libPhysX3CookingPROFILE.dylib",
            "code_id": "5e012a646cc536f19b4da0564049169b",
            "debug_file": "/Users/bruno/Documents/Unreal "
            "Projects/YetAnotherMac/MacNoEditor/YetAnotherMac.app/Contents/UE4/Engine/Binaries/ThirdParty/PhysX3/Mac/libPhysX3CookingPROFILE.dylib",
            "debug_id": "5e012a64-6cc5-36f1-9b4d-a0564049169b",
            "debug_status": "unused",
            "image_addr": "0x112fc0000",
            "image_size": 221183,
            "type": "macho",
        },
        {
            "arch": "unknown",
            "code_file": "/Users/bruno/Documents/Unreal "
            "Projects/YetAnotherMac/MacNoEditor/YetAnotherMac.app/Contents/UE4/Engine/Binaries/ThirdParty/PhysX3/Mac/libPhysX3CommonPROFILE.dylib",
            "code_id": "9c19854471943de6b67e4cc27eed2eab",
            "debug_file": "/Users/bruno/Documents/Unreal "
            "Projects/YetAnotherMac/MacNoEditor/YetAnotherMac.app/Contents/UE4/Engine/Binaries/ThirdParty/PhysX3/Mac/libPhysX3CommonPROFILE.dylib",
            "debug_id": "9c198544-7194-3de6-b67e-4cc27eed2eab",
            "debug_status": "unused",
            "image_addr": "0x113013000",
            "image_size": 1474559,
            "type": "macho",
        },
        {
            "arch": "unknown",
            "code_file": "/Users/bruno/Documents/Unreal "
            "Projects/YetAnotherMac/MacNoEditor/YetAnotherMac.app/Contents/UE4/Engine/Binaries/ThirdParty/PhysX3/Mac/libPxFoundationPROFILE.dylib",
            "code_id": "890f0997f90435449af7cf011f09a06e",
            "debug_file": "/Users/bruno/Documents/Unreal "
            "Projects/YetAnotherMac/MacNoEditor/YetAnotherMac.app/Contents/UE4/Engine/Binaries/ThirdParty/PhysX3/Mac/libPxFoundationPROFILE.dylib",
            "debug_id": "890f0997-f904-3544-9af7-cf011f09a06e",
            "debug_status": "unused",
            "image_addr": "0x1131fa000",
            "image_size": 28671,
            "type": "macho",
        },
    ],
    "stacktraces": [
        {
            "frames": [
                {
                    "instruction_addr": "0x7fff61bc6c2a",
                    "original_index": 0,
                    "package": "libsystem_kernel.dylib",
                    "status": "unknown_image",
                },
                {
                    "instruction_addr": "0x7fff349f505e",
                    "original_index": 1,
                    "package": "CoreFoundation",
                    "status": "unknown_image",
                },
                {
                    "instruction_addr": "0x7fff349f45ad",
                    "original_index": 2,
                    "package": "CoreFoundation",
                    "status": "unknown_image",
                },
                {
                    "instruction_addr": "0x7fff349f3ce4",
                    "original_index": 3,
                    "package": "CoreFoundation",
                    "status": "unknown_image",
                },
                {
                    "instruction_addr": "0x7fff33c8d895",
                    "original_index": 4,
                    "package": "HIToolbox",
                    "status": "unknown_image",
                },
                {
                    "instruction_addr": "0x7fff33c8d5cb",
                    "original_index": 5,
                    "package": "HIToolbox",
                    "status": "unknown_image",
                },
                {
                    "instruction_addr": "0x7fff33c8d348",
                    "original_index": 6,
                    "package": "HIToolbox",
                    "status": "unknown_image",
                },
                {
                    "instruction_addr": "0x7fff31f4a95b",
                    "original_index": 7,
                    "package": "AppKit",
                    "status": "unknown_image",
                },
                {
                    "instruction_addr": "0x7fff31f496fa",
                    "original_index": 8,
                    "package": "AppKit",
                    "status": "unknown_image",
                },
                {
                    "instruction_addr": "0x7fff31f4375d",
                    "original_index": 9,
                    "package": "AppKit",
                    "status": "unknown_image",
                },
                {
                    "instruction_addr": "0x108b7092b",
                    "original_index": 10,
                    "package": "/Users/bruno/Documents/Unreal "
                    "Projects/YetAnotherMac/MacNoEditor/YetAnotherMac.app/Contents/MacOS/YetAnotherMac",
                    "status": "missing",
                },
                {
                    "instruction_addr": "0x108b702a6",
                    "original_index": 11,
                    "package": "/Users/bruno/Documents/Unreal "
                    "Projects/YetAnotherMac/MacNoEditor/YetAnotherMac.app/Contents/MacOS/YetAnotherMac",
                    "status": "missing",
                },
                {
                    "instruction_addr": "0x7fff61a8e085",
                    "original_index": 12,
                    "package": "libdyld.dylib",
                    "status": "unknown_image",
                },
                {
                    "instruction_addr": "0xea004",
                    "original_index": 13,
                    "package": "YetanotherMac",
                    "status": "unknown_image",
                },
            ],
            "is_requesting": False,
            "thread_id": 0,
        },
        {
            "frames": [
                {
                    "instruction_addr": "0x7fff61bc85be",
                    "original_index": 0,
                    "package": "libsystem_kernel.dylib",
                    "status": "unknown_image",
                },
                {
                    "instruction_addr": "0x7fff61c7f415",
                    "original_index": 1,
                    "package": "libsystem_pthread.dylib",
                    "status": "unknown_image",
                },
                {
                    "instruction_addr": "0x54485244",
                    "original_index": 2,
                    "status": "unknown_image",
                },
            ],
            "is_requesting": True,
            "registers": {
                "cs": "0x2b",
                "fs": "0x0",
                "gs": "0x0",
                "r10": "0x0",
                "r11": "0xffffffff",
                "r12": "0x8",
                "r13": "0x11e800b00",
                "r14": "0x1",
                "r15": "0x0",
                "r8": "0x3",
                "r9": "0x10",
                "rax": "0x20261bb4775b008f",
                "rbp": "0x700015a616d0",
                "rbx": "0x0",
                "rcx": "0x1288266c0",
                "rdi": "0x0",
                "rdx": "0x1",
                "rflags": "0x10206",
                "rip": "0x1090a0132",
                "rsi": "0x0",
                "rsp": "0x700015a613f0",
            },
            "thread_id": 1,
        },
    ],
    "status": "completed",
    "system_info": {
        "cpu_arch": "x86_64",
        "device_model": "MacBookPro14,3",
        "os_build": "",
        "os_name": "Mac OS X 10.14.0 (18A391)",
        "os_version": "",
    },
    "timestamp": 1547055742,
}


def test_basic(symbolicator, hitcounter):
    service = symbolicator()
    service.wait_healthcheck()

    with open("tests/fixtures/apple_crash_report.txt", "rb") as f:
        response = service.post(
            "/applecrashreport",
            files={"apple_crash_report": f},
            data={"sources": json.dumps([])},
        )
        response.raise_for_status()

    from pprint import pprint

    pprint(response.json())

    assert response.json() == APPLE_CRASH_REPORT_SUCCESS
