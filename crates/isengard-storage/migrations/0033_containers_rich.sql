-- 2026-05-23: rich container detail for compose synthesis.
--
-- Phase 0.18 (migration 0029) added the slim `containers` table that
-- powers `isd ps`. That table carries the bits the operator's CLI
-- needs to render a container row, but not the bits a compose
-- synthesizer needs (ports, env, volumes, networks, restart policy,
-- healthcheck). Spec: 3 Resources/Superpowers/specs/2026-05-23-isd-compose-synthesize-design.md.
--
-- This migration lands a sibling `containers_rich` table keyed by the
-- same operator-visible container id. Heartbeat ingest upserts a row
-- when the agent ships a `ContainerInfo.rich` block (proto3 optional,
-- absent on older agents). The row is dropped automatically when the
-- containers row goes (ON DELETE CASCADE).
--
-- JSON columns are TEXT carrying JSON-encoded values:
--   ports_json      -> [{host_ip, host_port, container_port, protocol}, ...]
--   env_json        -> ["KEY=value", ...]
--   mounts_json     -> [{kind, source, target, read_only}, ...]
--   networks_json   -> ["frontend", "backend", ...]
--   command_json    -> ["nginx", "-g", "daemon off;"] (NULL = image default)
--   entrypoint_json -> ["/docker-entrypoint.sh"] (NULL = image default)
--   healthcheck_json -> {test:[...], interval_ns, timeout_ns, retries, start_period_ns}
--
-- The synthesizer never queries inside the JSON; it deserialises the
-- whole blob per container. SQLite's JSON1 stays available for ad-hoc
-- inspection but we don't depend on it.

CREATE TABLE containers_rich (
    container_id     TEXT      PRIMARY KEY REFERENCES containers(id) ON DELETE CASCADE,
    host_id          BLOB      NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    ports_json       TEXT      NOT NULL DEFAULT '[]',
    env_json         TEXT      NOT NULL DEFAULT '[]',
    mounts_json      TEXT      NOT NULL DEFAULT '[]',
    networks_json    TEXT      NOT NULL DEFAULT '[]',
    restart_policy   TEXT,
    command_json     TEXT,
    entrypoint_json  TEXT,
    working_dir      TEXT,
    user_spec        TEXT,
    healthcheck_json TEXT,
    updated_at       TIMESTAMP NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
);

CREATE INDEX containers_rich_by_host ON containers_rich(host_id);
