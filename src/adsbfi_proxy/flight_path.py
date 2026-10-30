"""Read one aircraft's observed positions from the mirrored half-hour files."""
import mmap
import struct
import threading
import time
from collections import OrderedDict

from .config import SLOT, iso

MARKER = struct.pack('<I', 0x0E7F7C9D)
SCANS = threading.BoundedSemaphore(2)
CACHE = OrderedDict()
CACHE_LOCK = threading.Lock()
GAP_SECONDS = 180


class PathBusy(Exception):
    pass


def read_positions(path, slot, icao, start, end, deadline):
    """Search mapped bytes in C, then decode only matching aligned records."""
    needle = struct.pack('<I', icao)[:3]
    points = []
    with path.open('rb') as source:
        size = source.seek(0, 2)
        if size < 32 or size % 16:
            raise ValueError('Invalid heatmap length')
        with mmap.mmap(source.fileno(), 0, access=mmap.ACCESS_READ) as raw:
            slices = struct.unpack_from('<I', raw)[0]
            if not 1 <= slices <= 1800 or slices * 16 + 16 > size:
                raise ValueError('Invalid heatmap header')
            offsets = []
            offset = slices * 16
            while (offset := raw.find(MARKER, offset)) != -1:
                if offset % 16 == 0:
                    offsets.append(offset)
                offset += 4
            if len(offsets) != slices or offsets[0] != slices * 16:
                raise ValueError('Incomplete heatmap slices')
            for index, offset in enumerate(offsets):
                if time.monotonic() > deadline:
                    raise TimeoutError('Path extraction timed out; try again when the disk is less busy')
                _, hi, lo, _ = struct.unpack_from('<4I', raw, offset)
                ts = round(((hi << 32) + lo) / 1000)
                if not slot <= ts < slot + SLOT:
                    raise ValueError('Heatmap timestamp outside its slot')
                if not start <= ts <= end:
                    continue
                stop = offsets[index + 1] if index + 1 < slices else size
                cursor = offset + 16
                while (cursor := raw.find(needle, cursor, stop)) != -1:
                    if cursor % 16 == 0 and cursor + 16 <= stop:
                        aircraft, lat, lon, altitude = struct.unpack_from('<Iiii', raw, cursor)
                        # Exclude non-ICAO addresses and non-position records.
                        if aircraft & 0x1FFFFFF == icao and -90000000 <= lat <= 90000000 and -180000000 <= lon <= 180000000:
                            signed = altitude & 65535
                            if signed & 32768:
                                signed -= 65536
                            points.append({'timestamp': ts, 'lat': lat / 1e6, 'lon': lon / 1e6,
                                           'altitude_ft': None if signed in (-123, -124) else signed * 25,
                                           'on_ground': signed == -123})
                    cursor += 3
    return points


def geometry(points):
    """Break at observation gaps; unwrap longitude for short dateline crossings."""
    segments = []
    current = []
    previous = None
    for point in points:
        lon = point['lon']
        if previous:
            lon += 360 * round((previous[0] - lon) / 360)
        if previous and point['timestamp'] - previous[2] > GAP_SECONDS:
            if current:
                segments.append(current)
            current = []
        current.append([lon, point['lat']])
        previous = (lon, point['lat'], point['timestamp'])
    if current:
        segments.append(current)
    features = []
    for segment in segments:
        shape = {'type': 'LineString', 'coordinates': segment} if len(segment) > 1 else {'type': 'Point', 'coordinates': segment[0]}
        features.append({'type': 'Feature', 'properties': {}, 'geometry': shape})
    coordinates = [point for segment in segments for point in segment]
    bounds = [[min(p[0] for p in coordinates), min(p[1] for p in coordinates)],
              [max(p[0] for p in coordinates), max(p[1] for p in coordinates)]] if coordinates else None
    return {'type': 'FeatureCollection', 'features': features}, bounds, max(0, len(segments) - 1)


def extract(config, flight):
    start, end, icao = flight['takeoff_ts'], flight['landing_ts'], flight['icao_id']
    if not 0 < end - start <= 30 * 3600:
        raise ValueError('Flight duration is outside the supported 30-hour range')
    slots = list(range(start // SLOT * SLOT, end // SLOT * SLOT + SLOT, SLOT))
    inventory = []
    for slot in slots:
        path = config.locate(slot)
        try:
            stat = path.stat()
            inventory.append((slot, stat.st_size, stat.st_mtime_ns))
        except FileNotFoundError:
            inventory.append((slot, None, None))
    key = (str(config.cache), icao, start, end, tuple(inventory))
    with CACHE_LOCK:
        if key in CACHE:
            CACHE.move_to_end(key)
            return CACHE[key]
    if not SCANS.acquire(blocking=False):
        raise PathBusy('Other flight paths are loading; try again in a moment')
    try:
        points, missing, unreadable = [], [], []
        deadline = time.monotonic() + 45
        for slot, size, _ in inventory:
            if size is None:
                missing.append(iso(slot))
                continue
            try:
                points.extend(read_positions(config.locate(slot), slot, icao, start, end, deadline))
            except FileNotFoundError:
                missing.append(iso(slot))
            except TimeoutError:
                raise
            except (ValueError, OSError):
                unreadable.append(iso(slot))
        # Files are chronological; deduplicate any repeated observation timestamps.
        points = sorted({p['timestamp']: p for p in points}.values(), key=lambda p: p['timestamp'])
        geojson, bounds, gaps = geometry(points)
        partial = bool(missing or unreadable or gaps or (points and (points[0]['timestamp'] - start > GAP_SECONDS or end - points[-1]['timestamp'] > GAP_SECONDS)))
        result = {'points': points, 'geojson': geojson, 'bounds': bounds,
                  'coverage': 'unavailable' if not points else 'partial' if partial else 'available',
                  'missing_slots': missing, 'unreadable_slots': unreadable,
                  'gap_count': gaps, 'gap_threshold_seconds': GAP_SECONDS,
                  'source': 'local_heatmap_cache'}
        if points:
            with CACHE_LOCK:
                CACHE[key] = result
                CACHE.move_to_end(key)
                while len(CACHE) > 32:
                    CACHE.popitem(last=False)
        return result
    finally:
        SCANS.release()
