# TSDB Migration

This file keeps only the current migration state, the actions still required to complete migration, and the repo-local backlog that remains once migration is complete.

## Current state

The repo-local runtime cutover is complete.

z2m-hub already runs PostgreSQL-first in production and has no remaining repo-local runtime dependency on InfluxDB. The only migration-critical work left from this repo's perspective is the shared Phase 5 InfluxDB shutdown on `pi5data`, tracked in `~/github/energy-hub/lat.md/tsdb-migration.md`.

## Actions required to complete migration

The broader migration is not complete until the shared platform finishes Phase 5.

1. retire Telegraf's `influxdb_v2` output
2. remove the Grafana v2 datasource
3. stop and remove the InfluxDB v2 container
4. archive the v2 data volume

## New work backlog once migration is done

These are repo-local closeout actions after the shared tracker closes and the rollback window is explicitly over.

1. Remove `pi5data:/usr/local/bin/z2m-hub.pre-pg-rollback.bak`.
2. Remove the temporary Influx note from [[infrastructure#Hosts and roles]].
3. Retire this file or reduce it to a short backlink once no closure note still needs to live here.
4. If `[[tsdb-migration]]` stops being a first-class node, replace or remove the backlink from [[lat#Knowledge map]].

Stable PostgreSQL-first behaviour lives in:
- [[architecture#Runtime structure]]
- [[interfaces#PostgreSQL read and write contract]]
- [[dhw#Capacity autoload and persistence]] and [[dhw#Restart recovery]]
- [[infrastructure#Deployment and configuration]] and [[infrastructure#Secret management]]
- [[tests#PostgreSQL interface]] and [[tests#Real PostgreSQL integration]]
