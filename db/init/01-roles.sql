-- Bootstrap roles for local dev (ADR-007).
-- Idempotent: safe to re-run inside docker-entrypoint-initdb.d (first-boot only)
-- and via `make db-bootstrap` against an existing cluster.

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'velocity_api') THEN
        CREATE ROLE velocity_api LOGIN PASSWORD 'velocity_api_dev'
            NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOINHERIT;
    END IF;
END
$$;

ALTER ROLE velocity_api NOSUPERUSER NOBYPASSRLS;

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'velocity_operator') THEN
        CREATE ROLE velocity_operator LOGIN PASSWORD 'velocity_operator_dev'
            NOSUPERUSER NOBYPASSRLS CREATEROLE;
    END IF;
END
$$;

GRANT CONNECT ON DATABASE velocity TO velocity_api, velocity_operator;

-- velocity_operator owns per-domain schemas (CREATE SCHEMA, CREATE ROLE for
-- the reader/writer/admin domain roles). It is still NOSUPERUSER + NOBYPASSRLS;
-- this is the bounded "platform admin" surface.
GRANT CREATE ON DATABASE velocity TO velocity_operator;
