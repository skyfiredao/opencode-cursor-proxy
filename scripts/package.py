import argparse
import json
from pathlib import Path
import struct
import tarfile
import zipfile


TARGETS = {
    "x86_64-pc-windows-msvc": ("windows-amd64", "cursor-proxy.exe", ".zip"),
    "aarch64-apple-darwin": ("macos-arm64", "cursor-proxy", ".tar.gz"),
    "aarch64-unknown-linux-musl": ("linux-arm64-musl", "cursor-proxy", ".tar.gz"),
    "x86_64-unknown-linux-musl": ("linux-amd64-musl", "cursor-proxy", ".tar.gz"),
}


def inspect_binary(binary, target):
    data = binary.read_bytes()
    if target.endswith("linux-musl"):
        if len(data) < 64 or data[:6] != b"\x7fELF\x02\x01":
            raise ValueError("Linux package requires a little-endian ELF64 binary")
        machine = struct.unpack_from("<H", data, 18)[0]
        expected = 183 if target.startswith("aarch64") else 62
        if machine != expected:
            raise ValueError(f"ELF architecture {machine} does not match {target}")
        offset = struct.unpack_from("<Q", data, 32)[0]
        entry_size, count = struct.unpack_from("<HH", data, 54)
        if not count or entry_size < 56 or offset + entry_size * count > len(data):
            raise ValueError("ELF program header table is missing or truncated")
        for index in range(count):
            header = offset + index * entry_size
            segment = struct.unpack_from("<I", data, header)[0]
            if segment == 3:
                raise ValueError("Linux musl package must be static: PT_INTERP is present")
            if segment == 2:
                start = struct.unpack_from("<Q", data, header + 8)[0]
                size = struct.unpack_from("<Q", data, header + 32)[0]
                if size % 16 or start + size > len(data):
                    raise ValueError("ELF dynamic segment is truncated")
                for position in range(start, start + size, 16):
                    tag = struct.unpack_from("<q", data, position)[0]
                    if tag == 0:
                        break
                    if tag == 1:
                        raise ValueError("Linux musl package must be static: DT_NEEDED is present")
    elif target.endswith("windows-msvc"):
        if len(data) < 64 or data[:2] != b"MZ":
            raise ValueError("Windows package requires a PE executable")
        offset = struct.unpack_from("<I", data, 60)[0]
        if offset + 26 > len(data) or data[offset:offset + 4] != b"PE\0\0":
            raise ValueError("PE header is missing or truncated")
        machine = struct.unpack_from("<H", data, offset + 4)[0]
        optional_magic = struct.unpack_from("<H", data, offset + 24)[0]
        if machine != 0x8664 or optional_magic != 0x20B:
            raise ValueError("Windows package requires an amd64 PE32+ executable")
    elif target.endswith("apple-darwin"):
        if len(data) < 32 or struct.unpack_from("<II", data) != (0xFEEDFACF, 0x0100000C):
            raise ValueError("macOS package requires an arm64 Mach-O executable")


def main():
    parser = argparse.ArgumentParser(
        description="Package one explicitly targeted Release binary without running it."
    )
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--output", type=Path, default=Path("dist"))
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    inspect_binary(binary, args.target)
    platform, executable, suffix = TARGETS[args.target]
    args.output.mkdir(parents=True, exist_ok=True)
    archive_path = args.output / f"cursor-proxy-{platform}{suffix}"
    if suffix == ".zip":
        with zipfile.ZipFile(archive_path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
            archive.write(binary, arcname=executable)
    else:
        with tarfile.open(archive_path, "w:gz", compresslevel=9) as archive:
            info = archive.gettarinfo(str(binary), arcname=executable)
            info.mode = 0o755
            with binary.open("rb") as source:
                archive.addfile(info, source)
    result = {
        "target": args.target,
        "archive": archive_path.name,
        "binary_bytes": binary.stat().st_size,
        "archive_bytes": archive_path.stat().st_size,
    }
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
