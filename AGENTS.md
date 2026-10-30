# Development

Endpoint ledger v5 is canonical; no pjet or paired-flight compatibility is required.
Use fresh versioned databases for inference changes. Preserve rolling track state
across restarts, and never advance a checkpoint before endpoint rows are committed. Backfills must remain rate-limited and resumable.

Generated databases, raw heatmaps, reference inputs, credentials, and deployment
configuration stay outside Git. Test inference changes against realistic inputs;
never reset or rebuild a production database as a routine test.

The aircraft-embedding app consumes these databases but owns its embeddings.
Do not generate, train, or replace embedding artifacts as part of proxy updates.

Run the README checks before deployment. Stop other ingestion services before
switching a database to this worker. Read-only consumers may remain running.
