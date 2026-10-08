"""Inventory normal and delay imports directly from the measured PE artifact."""
import json
import os
from pathlib import Path
import struct
import sys


def inventory(path):
    data = Path(path).read_bytes()
    pe = struct.unpack_from('<I', data, 0x3C)[0]
    if data[pe:pe + 4] != b'PE\0\0':
        raise ValueError('not a PE image')
    sections, optional_size = struct.unpack_from('<H12xH', data, pe + 6)
    optional = pe + 24
    magic = struct.unpack_from('<H', data, optional)[0]
    if magic != 0x20B:
        raise ValueError('expected PE32+ (64-bit)')
    image_base = struct.unpack_from('<Q', data, optional + 24)[0]
    directory = optional + 112
    headers = struct.unpack_from('<I', data, optional + 60)[0]
    mappings = []
    for index in range(sections):
        entry = optional + optional_size + index * 40
        virtual_size, rva, raw_size, raw = struct.unpack_from('<IIII', data, entry + 8)
        mappings.append((rva, max(virtual_size, raw_size), raw))

    def offset(rva):
        if rva < headers:
            return rva
        for start, size, raw in mappings:
            if start <= rva < start + size:
                return raw + rva - start
        raise ValueError(f'unmapped RVA {rva:#x}')

    def string(rva):
        start = offset(rva)
        return data[start:data.index(b'\0', start)].decode('ascii')

    def thunks(rva):
        result = []
        if not rva:
            return result
        cursor = offset(rva)
        while True:
            value = struct.unpack_from('<Q', data, cursor)[0]
            if not value:
                return result
            result.append({'ordinal': value & 0xFFFF} if value >> 63 else {'name': string(value + 2)})
            cursor += 8

    def descriptors(number, delay):
        rva, size = struct.unpack_from('<II', data, directory + 8 * number)
        if not rva:
            return []
        cursor = offset(rva)
        result = []
        while True:
            values = struct.unpack_from('<8I' if delay else '<5I', data, cursor)
            if not any(values):
                return result
            if delay:
                attributes, name, _, iat, lookup, _, _, _ = values
                if not attributes & 1:
                    name -= image_base
                    lookup -= image_base
                    iat -= image_base
            else:
                lookup, _, _, name, iat = values
            result.append({'dll': string(name), 'symbols': thunks(lookup or iat)})
            cursor += 32 if delay else 20

    return {'image': str(path), 'normal': descriptors(1, False), 'delay': descriptors(13, True)}


if __name__ == '__main__':
    report = inventory(sys.argv[1])
    failures = []
    for group in ('normal', 'delay'):
        for library in report[group]:
            dll = library['dll']
            api_set = dll.lower().startswith(('api-ms-', 'ext-ms-'))
            library['system32_file'] = str(Path(os.environ['SystemRoot']) / 'System32' / dll)
            library['api_set_contract'] = api_set
            if '/' in dll or '\\' in dll or not (api_set or Path(library['system32_file']).is_file()):
                failures.append(dll)
            print(f'{group}: {dll}: {len(library["symbols"])} symbols')
    report['non_system32_imports'] = failures
    Path(sys.argv[2]).write_text(json.dumps(report, indent=2), encoding='utf-8')
    print(f'imported DLLs: {len(report["normal"])} normal, {len(report["delay"])} delay')
    if failures:
        sys.exit(f'non-System32 imports: {failures}')
