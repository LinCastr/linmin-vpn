#!/usr/bin/env python3
# Decodes a pipespy log of the Radmin VPN GUI<->service named pipe into its TLV
# attribute tree. Reverse-engineering helper; see docs/PROTOCOL.md. Not part of
# the app. Usage: rvdecode.py spy.log
"""Decode a pipespy log of the Radmin VPN GUI<->service pipe (development tool)."""
import re, sys, struct

def frames(data):
    i = 0
    while i + 4 <= len(data):
        n = struct.unpack_from('<I', data, i)[0]
        yield data[i + 4:i + 4 + n]
        i += 4 + n

def attrs(b, depth, out):
    i = 0
    while i + 8 <= len(b):
        ln, typ, flg, tag = struct.unpack_from('>IBBH', b, i)
        v = b[i + 8:i + 8 + ln]
        pad = '  ' * depth
        if typ == 0:
            out.append(f'{pad}{tag:04x} {{')
            attrs(v, depth + 1, out)
            out.append(f'{pad}}}')
        elif typ == 1 and ln == 4:
            out.append(f'{pad}{tag:04x} u32 {struct.unpack(">I", v)[0]}')
        elif typ == 2 and ln == 8:
            out.append(f'{pad}{tag:04x} u64 {struct.unpack(">Q", v)[0]:#x}')
        elif typ == 0x0b and ln == 4:
            out.append(f'{pad}{tag:04x} b32 {struct.unpack(">I", v)[0]}')
        elif typ == 3:
            s = v[:ln - (ln % 2)]
            try:
                txt = s.decode('utf-16-be')
            except Exception:
                txt = s.hex()
            out.append(f'{pad}{tag:04x} str({flg}) {txt!r}')
        else:
            out.append(f'{pad}{tag:04x} t{typ:02x}/{flg} [{ln}] {v.hex()}')
        i += 8 + ln
    if i != len(b):
        out.append(f'{"  " * depth}!! trailing {b[i:].hex()}')

def main(path):
    # Each direction is a byte stream: a frame may arrive in several reads
    # (the GUI writes the length and the body separately).
    streams = {'GUI->SVC': bytearray(), 'SVC->GUI': bytearray()}
    cur = None
    for line in open(path, errors='replace'):
        m = re.match(r'(\S+) (GUI->SVC|SVC->GUI) (\d+)', line)
        if m:
            cur = m.group(2)
            ts = m.group(1)
            continue
        m = re.match(r'  [0-9a-f]{4} ((?:[0-9a-f]{2} )+)', line)
        if m and cur:
            st = streams[cur]
            st += bytes.fromhex(m.group(1).replace(' ', ''))
            while len(st) >= 4:
                n = struct.unpack_from('<I', st, 0)[0]
                if len(st) < 4 + n:
                    break
                f = bytes(st[4:4 + n])
                del st[:4 + n]
                out = []
                attrs(f, 1, out)
                print(f'== {ts} {cur} ({n})')
                print('\n'.join(out))

main(sys.argv[1])
