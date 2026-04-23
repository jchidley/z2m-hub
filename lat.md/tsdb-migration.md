# TSDB Migration

This file keeps only the current migration state, the actions still required to complete migration, and the repo-local backlog that remains once migration is complete.

## Current state

The repo-local runtime cutover is complete, and the shared platform shutdown is complete too.

z2m-hub already runs PostgreSQL-first in production, has no remaining repo-local runtime dependency on InfluxDB, and no longer depends on any live InfluxDB service on `pi5data`.

## Actions required to complete migration

No migration-critical actions remain.

## New work backlog once migration is done

These are repo-local cleanup items that can still happen after the completed shared shutdown.

1. Retire this file or reduce it to a short backlink once no closure note still needs to live here.
2. If `[[tsdb-migration]]` stops being a first-class node, replace or remove the backlink from [[lat#Knowledge map]].

Stable PostgreSQL-first behaviour lives in:
- [[architecture#Runtime structure]]
- [[interfaces#PostgreSQL read and write contract]]
- [[dhw#Capacity autoload and persistence]] and [[dhw#Restart recovery]]
- [[infrastructure#Deployment and configuration]] and [[infrastructure#Secret management]]
- [[tests#PostgreSQL interface]] and [[tests#Real PostgreSQL integration]]
