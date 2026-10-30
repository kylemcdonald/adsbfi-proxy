"""Shared configuration. Runtime has no third-party Python dependencies."""
import os
from dataclasses import dataclass
from pathlib import Path
from datetime import datetime, timezone

ROOT = Path(__file__).resolve().parents[2]
SLOT = 1800

def timestamp(value):
    parsed = datetime.fromisoformat(value.replace('Z', '+00:00'))
    return int((parsed if parsed.tzinfo else parsed.replace(tzinfo=timezone.utc)).timestamp())

def iso(ts):
    return datetime.fromtimestamp(ts, timezone.utc).isoformat().replace('+00:00', 'Z')

def slot_relative(ts):
    # The same layout as the upstream globe_history tree.
    dt = datetime.fromtimestamp(ts, timezone.utc)
    return f"{dt.strftime('%Y/%m/%d')}/heatmap/{dt.hour * 2 + dt.minute // 30:02d}.bin.ttf"

def slot_path(cache, ts):
    return cache / slot_relative(ts)

def slot_ts(relative):
    """Slot start for a path relative to the cache root."""
    parts = Path(relative).parts
    return timestamp('-'.join(parts[:3])) + int(parts[-1].split('.')[0]) * SLOT

def latest_slot(now):
    # Allow two minutes for publication after the half-hour closes.
    return (int(now) - SLOT - 120) // SLOT * SLOT

@dataclass
class Config:
    db: Path
    cache: Path
    builder: Path
    mode: str = 'live'
    start: int | None = None
    mirror_start: int | None = None
    interval: float = 60
    request_interval: float = 10
    min_free_bytes: int = 5 * 1024**3
    base_url: str = 'https://globe.adsb.fi/globe_history'
    fallback_url: str = ''
    archive: Path | None = None

    def locate(self, ts):
        """The cached file for a slot, else the read-only archive copy, else the cache path."""
        path = slot_path(self.cache, ts)
        if self.archive is not None and not path.exists():
            archived = slot_path(self.archive, ts)
            if archived.exists():
                return archived
        return path

    @classmethod
    def env(cls):
        data = Path(os.getenv('ADSBFI_DATA_DIR', ROOT / 'data'))
        return cls(
            db=Path(os.getenv('ADSBFI_DB_DIR', data / 'db')).expanduser().resolve(),
            cache=Path(os.getenv('ADSBFI_CACHE_DIR', data / 'heatmaps')).expanduser().resolve(),
            builder=Path(os.getenv('ADSBFI_BUILDER', ROOT / 'rust/flight-builder/target/release/flight-builder')).expanduser().resolve(),
            mode=os.getenv('ADSBFI_MODE', 'live'),
            start=timestamp(os.environ['ADSBFI_START']) if os.getenv('ADSBFI_START') else None,
            mirror_start=timestamp(os.environ['ADSBFI_MIRROR_START']) if os.getenv('ADSBFI_MIRROR_START') else None,
            interval=float(os.getenv('ADSBFI_POLL_SECONDS', '60')),
            request_interval=float(os.getenv('ADSBFI_REQUEST_SECONDS', '10')),
            min_free_bytes=int(float(os.getenv('ADSBFI_MIN_FREE_GB', '5')) * 1024**3),
            base_url=os.getenv('ADSBFI_BASE_URL', 'https://globe.adsb.fi/globe_history').rstrip('/'),
            fallback_url=os.getenv('ADSBFI_FALLBACK_URL', '').rstrip('/'),
            archive=Path(os.environ['ADSBFI_ARCHIVE_DIR']).expanduser().resolve() if os.getenv('ADSBFI_ARCHIVE_DIR') else None,
        )

    def validate(self):
        if self.mode not in ('live', 'mirror'):
            raise ValueError('ADSBFI_MODE must be live or mirror')
        if self.interval < 1 or self.request_interval < 1:
            raise ValueError('poll and request intervals must be at least one second')
        for name in ('airports.sqlite', 'aircraft.sqlite'):
            if not (self.db / name).is_file():
                raise ValueError(f'Missing reference database: {self.db / name}')
        if not self.builder.is_file():
            raise ValueError(f'Missing builder: {self.builder}')
