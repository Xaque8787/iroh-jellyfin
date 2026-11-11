"""Verify ELF identity, JNI export and minimum page alignment for every shipped ABI."""
import os, pathlib, re, struct, subprocess, sys
root = pathlib.Path(sys.argv[1])
for abi, machine in {"arm64-v8a": 183, "armeabi-v7a": 40, "x86_64": 62}.items():
    path = root / abi / "libiroh_client.so"
    data = path.read_bytes()
    assert data[:4] == b"\x7fELF" and data[5] == 1, path
    assert struct.unpack_from("<H", data, 18)[0] == machine, path
    if data[4] == 2:
        off = struct.unpack_from("<Q", data, 32)[0]
        size, count = struct.unpack_from("<HH", data, 54)
        align_offset, align_format = 48, "<Q"
    else:
        off = struct.unpack_from("<I", data, 28)[0]
        size, count = struct.unpack_from("<HH", data, 42)
        align_offset, align_format = 28, "<I"
    for i in range(count):
        pos = off + i * size
        if struct.unpack_from("<I", data, pos)[0] == 1:
            assert struct.unpack_from(align_format, data, pos + align_offset)[0] >= 16384, path
    for name in ("invoke", "initializeTls"):
        assert ("Java_io_github_xaque8787_iroh_NativeBridge_" + name).encode() + b"\0" in data, path
    readelf = next(pathlib.Path(os.environ["ANDROID_NDK_HOME"]).glob("toolchains/llvm/prebuilt/*/bin/llvm-readelf"))
    dynamic = subprocess.check_output([str(readelf), "-d", str(path)], text=True)
    needed = set(re.findall(r"Shared library: \[([^\]]+)\]", dynamic))
    assert needed <= {"libc.so", "libdl.so", "libm.so", "liblog.so", "libandroid.so"}, (path, needed)
    print(f"verified {abi}: ELF machine {machine}, JNI symbols, 16 KiB alignment, OS-only native dependencies")
