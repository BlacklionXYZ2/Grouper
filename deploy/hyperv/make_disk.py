"""Helpers for new-hub-vm.ps1 (stdlib + pycdlib only).

    python make_disk.py extract <image.tar.xz> <out.raw>
    python make_disk.py vhd <disk.raw>                 # append a fixed-VHD footer in place
    python make_disk.py seed <out.iso> <user-data> <meta-data>
"""

import io
import os
import struct
import sys
import tarfile
import time
import uuid


def extract(archive: str, out: str) -> None:
    with tarfile.open(archive, "r:xz") as tar:
        member = next(m for m in tar.getmembers() if m.isfile() and m.name.endswith(".raw"))
        src = tar.extractfile(member)
        with open(out, "wb") as dst:
            while chunk := src.read(16 << 20):
                # Skip writing all-zero blocks so the file stays sparse where the FS allows.
                if chunk.count(0) == len(chunk):
                    dst.seek(len(chunk), os.SEEK_CUR)
                else:
                    dst.write(chunk)
            dst.truncate()


def chs(size: int) -> tuple[int, int, int]:
    """Disk geometry as defined in the VHD specification."""
    total = min(size // 512, 65535 * 16 * 255)
    if total >= 65535 * 16 * 63:
        spt, heads = 255, 16
        cth = total // spt
    else:
        spt = 17
        cth = total // spt
        heads = max((cth + 1023) // 1024, 4)
        if cth >= heads * 1024 or heads > 16:
            spt, heads = 31, 16
            cth = total // spt
        if cth >= heads * 1024:
            spt, heads = 63, 16
            cth = total // spt
    return cth // heads, heads, spt


def vhd(path: str) -> None:
    size = os.path.getsize(path)
    if size % 512:
        raise SystemExit(f"{path}: size {size} is not a multiple of 512")
    cyl, heads, spt = chs(size)
    stamp = int(time.time()) - 946684800  # seconds since 2000-01-01
    footer = bytearray(512)
    struct.pack_into(
        ">8sIIQI4sI4sQQHBBII16sB",
        footer,
        0,
        b"conectix",
        2,  # features
        0x00010000,  # format version
        0xFFFFFFFFFFFFFFFF,  # data offset: none for a fixed disk
        stamp,
        b"grpr",
        0x00010000,
        b"Wi2k",
        size,
        size,
        cyl,
        heads,
        spt,
        2,  # fixed disk
        0,  # checksum, filled in below
        uuid.uuid4().bytes,
        0,
    )
    checksum = (~sum(footer)) & 0xFFFFFFFF
    struct.pack_into(">I", footer, 64, checksum)
    with open(path, "ab") as f:
        f.write(footer)


def seed(out: str, user_data: str, meta_data: str) -> None:
    import pycdlib

    iso = pycdlib.PyCdlib()
    iso.new(interchange_level=3, joliet=3, rock_ridge="1.09", vol_ident="cidata")
    for local, name in ((user_data, "user-data"), (meta_data, "meta-data")):
        data = open(local, "rb").read()
        short = name.upper().replace("-", "_")[:8] + ".;1"
        iso.add_fp(io.BytesIO(data), len(data), "/" + short, rr_name=name, joliet_path="/" + name)
    iso.write(out)
    iso.close()


if __name__ == "__main__":
    cmd, *args = sys.argv[1:]
    {"extract": extract, "vhd": vhd, "seed": seed}[cmd](*args)
