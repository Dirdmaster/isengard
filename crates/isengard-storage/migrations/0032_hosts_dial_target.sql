-- PR B of the isd ssh hosts UX overhaul: dial target column on hosts.
--
-- The dial target is the address an operator types into `ssh` to reach
-- this host (e.g. `dirdmaster@10.17.0.125`). Today `isd ssh hosts` only
-- knows the agent's reported hostname, which is the container hash on
-- docker-in-docker setups and useless for dialing. The CLI now captures
-- the operator's active docker context URL at enroll time and PATCHes
-- it onto the host row; operators can override via
-- `isd ssh hosts set <agent> --dial <target>`.
--
-- Nullable: pre-migration host rows keep `dial_target = NULL`. The CLI
-- renders NULL as `(unset)` so the column never collapses.

ALTER TABLE hosts ADD COLUMN dial_target TEXT;
