#!/usr/bin/env python3
"""Build a real APK: binary AndroidManifest.xml (AXML) + classes.dex.

Used to exercise apknative's full APK path — ZIP reader, manifest parser,
launcher-activity resolution and the interpreter — against a genuine APK rather
than a bare DEX.

Layout notes (from the AOSP ResXMLTree structures):
  ResXMLTree_node  = chunk header(8) + lineNumber(4) + comment(4)   = 16 bytes
  ResXMLTree_attrExt = ns(4) + name(4) + attributeStart(2) + attributeSize(2)
                       + attributeCount(2) + idIndex(2) + classIndex(2)
                       + styleIndex(2)                                = 20 bytes
  ResXMLTree_attribute = ns(4) + name(4) + rawValue(4)
                         + Res_value{size(2),res0(1),dataType(1),data(4)} = 20 bytes
so a start-element chunk is 16 + 20 + attributeCount*20 bytes.
"""
import struct
import sys
import zipfile

RES_STRING_POOL = 0x0001
RES_XML_TYPE = 0x0003
RES_XML_START_NS = 0x0100
RES_XML_END_NS = 0x0101
RES_XML_START_EL = 0x0102
RES_XML_END_EL = 0x0103
UTF8_FLAG = 1 << 8
ANDROID_NS = "http://schemas.android.com/apk/res/android"
TYPE_STRING = 0x03


class Pool:
    def __init__(self):
        self.strings = []

    def add(self, s):
        if s not in self.strings:
            self.strings.append(s)
        return self.strings.index(s)

    def encode(self):
        data = bytearray()
        offsets = []
        for s in self.strings:
            b = s.encode("utf-8")
            offsets.append(len(data))
            # UTF-8 pools store char-length then byte-length, each 1 or 2 bytes.
            data += bytes([len(s), len(b)]) + b + b"\x00"
        header_size = 0x1C
        strings_start = header_size + 4 * len(self.strings)
        size = strings_start + len(data)
        out = struct.pack("<HHIIIIII", RES_STRING_POOL, header_size, size,
                          len(self.strings), 0, UTF8_FLAG, strings_start, 0)
        for o in offsets:
            out += struct.pack("<I", o)
        return out + bytes(data)


def chunk(ctype, payload, header_size):
    # `size` counts the whole chunk from its first byte: the 8-byte
    # ResChunk_header plus everything after it. headerSize only records where
    # the type-specific data begins, so it is not part of the size.
    return struct.pack("<HHI", ctype, header_size, 8 + len(payload)) + payload


def collect(pool, elems):
    """Every string must be in the pool before it is encoded."""
    for name, attrs, children in elems:
        pool.add(name)
        for aname, aval in attrs:
            pool.add(aname)
            pool.add(aval)
        collect(pool, children)


def build_manifest(elems, pool):
    collect(pool, elems)
    ns_idx = pool.add(ANDROID_NS)

    out = bytearray(struct.pack("<HHI", RES_XML_TYPE, 8, 0))
    out += pool.encode()
    # ResXMLTree_namespace = { lineNumber, comment, prefix, uri }
    out += chunk(RES_XML_START_NS, struct.pack("<IIII", 0, 0, ns_idx, ns_idx), 0x10)

    def emit(nodes):
        nonlocal out
        for name, attrs, children in nodes:
            name_idx = pool.add(name)
            attr_bytes = b""
            count = 0
            for aname, aval in attrs:
                ai, vi = pool.add(aname), pool.add(aval)
                attr_bytes += struct.pack(
                    "<IIIHBBI", ns_idx, ai, vi, 8, 0, TYPE_STRING, vi)
                count += 1
            payload = struct.pack("<II", 0, 0)          # lineNumber, comment
            payload += struct.pack("<II", ns_idx, name_idx)
            payload += struct.pack("<HHHHHH", 20, 20, count, 0, 0, 0)
            payload += attr_bytes
            out += chunk(RES_XML_START_EL, payload, 0x10)
            if children:
                emit(children)
            out += chunk(RES_XML_END_EL, struct.pack("<IIII", 0, 0, ns_idx, name_idx), 0x10)

    emit(elems)
    out += chunk(RES_XML_END_NS, struct.pack("<IIII", 0, 0, ns_idx, ns_idx), 0x10)
    struct.pack_into("<I", out, 4, len(out))
    return bytes(out)


def main():
    dex_path, out_path, package, activity = (
        sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4])
    dex = open(dex_path, "rb").read()

    pool = Pool()
    action = ("action", [("name", "android.intent.action.MAIN")], [])
    category = ("category", [("name", "android.intent.category.LAUNCHER")], [])
    intent_filter = ("intent-filter", [], [action, category])
    act = ("activity", [("name", activity), ("exported", "true")], [intent_filter])
    app = ("application", [("label", "Demo")], [act])
    uses_sdk = ("uses-sdk", [("minSdkVersion", "21"), ("targetSdkVersion", "34")], [])
    root = ("manifest",
            [("package", package), ("versionCode", "1"), ("versionName", "1.0")],
            [uses_sdk, app])

    manifest = build_manifest([root], pool)

    with zipfile.ZipFile(out_path, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("AndroidManifest.xml", manifest)
        z.writestr("classes.dex", dex)
        z.writestr("resources.arsc", b"\x02\x00\x0c\x00" + b"\x00" * 64)
    print(f"wrote {out_path} ({package}, launcher {activity}, manifest {len(manifest)}B)")


if __name__ == "__main__":
    main()