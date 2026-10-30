# Upstream history gate (October 2026)

## Symptom

`https://globe.adsbexchange.com/globe_history/...` now returns:

- `403 Historical data requires a verified session.` with a browser-like referer
- `403 Direct access to globe history data is not permitted.` otherwise

The live `re-api` feed is gated too. Signing in to a normal ADSB Exchange
account sets `adsbx_identity` / `adsbx_identity_exp`, but the heatmap endpoint
still returns 403; the page's `POST /turnstile-token` session cannot be minted
by automated browsers.

## Workaround

`globe.adsb.fi` publishes the same readsb `bin.ttf` slices at the same UTC
layout:

```text
https://globe.adsb.fi/globe_history/YYYY/MM/DD/heatmap/HH.bin.ttf
```

The worker tries `ADSBFI_BASE_URL` first and falls back to
`ADSBFI_FALLBACK_URL` (`https://globe.adsb.fi/globe_history` by default) on
`403`, `404`, or a network error. It honours `429 Retry-After` globally and
records the serving source in `proxy-status.json` as `last_download_source`.

Set `ADSBFI_FALLBACK_URL=` to disable the mirror. Set both URLs to use another
readsb `globe_history` instance.

## Impact

The mirror is an independent community network, so coverage differs from
ADS-B Exchange. Files stay in the standard format; no parser, schema, or
inference changes are needed. Recent live ingestion and the rolling builder
continue, and old archive holes that neither source has remain queued.

## Reverting

If the gate is removed or a verified session becomes available, clear
`ADSBFI_FALLBACK_URL` or point it at the new source and restart the worker.
Watch `last_download_source` for a few cycles before removing the fallback.
