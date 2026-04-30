CREATE TABLE hosts (
    id              BLOB     PRIMARY KEY,
    fingerprint     TEXT     NOT NULL UNIQUE,
    hostname        TEXT     NOT NULL,
    os              TEXT     NOT NULL,
    arch            TEXT     NOT NULL,
    agent_version   TEXT     NOT NULL,
    docker_version  TEXT     NOT NULL,
    enrolled_at     INTEGER  NOT NULL,
    last_seen_at    INTEGER,
    metadata        TEXT     NOT NULL DEFAULT '{}'
);

CREATE INDEX hosts_last_seen_at_idx ON hosts(last_seen_at DESC);
