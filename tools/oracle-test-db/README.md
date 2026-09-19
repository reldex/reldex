# Oracle 19c local test database

A disposable, local Oracle Database 19c instance in Docker for Phase 0 driver
integration tests. **Local development only — not for production, not for
anything security-sensitive.**

Image: [`doctorkirk/oracle-19c:19.3`](https://hub.docker.com/r/doctorkirk/oracle-19c)
— a community image (not published or maintained by Oracle Corporation),
built from Oracle's official `oracle/docker-images` SingleInstance 19.3
scripts. Single-Instance, **Non-CDB**. The tag is pinned to `19.3`
deliberately; never move this to `latest`.

## Prerequisites

- Docker Desktop (Linux engine) with Docker Compose v2+.
- Roughly 10 GB of free disk for the image plus a few GB for the data
  volume (see **Resource usage observed** below for exact numbers).
- No other process bound to `127.0.0.1:1521` on the host.

## First start

```bash
cd tools/oracle-test-db
cp .env.example .env      # REQUIRED: replace every CHANGE_ME_... placeholder
docker compose pull       # ~2.3 GB compressed download
docker compose up -d
```

On first start the container runs Oracle's DBCA to create the database from
scratch. Oracle's own documentation quotes **10–25 minutes**; on this
16-CPU/42 GB host it completed in about **8.5 minutes**. Subsequent
`docker compose up -d` runs (with the data volume already populated) start
in well under a minute.

### Checking readiness

Tail the container logs and wait for the banner line:

```bash
docker compose logs -f oracle19c
# ... wait for:
# #########################
# DATABASE IS READY TO USE!
# #########################
```

Or poll the compose/Docker health status (the healthcheck wraps the image's
own `/opt/oracle/checkDBStatus.sh`, `start_period: 30m`):

```bash
docker compose ps
# STATUS column shows "(healthy)" once ready
```

## Connection details

Verified against this image after a successful first start:

| | |
|---|---|
| Host | `127.0.0.1` |
| Port | `1521` |
| SID | `RELDEX` |
| Service names | `RELDEX`, `RELDEXXDB` (from `lsnrctl status`) |
| CDB? | **No** — `select cdb from v$database;` returns `NO` |

Tested over the listener from inside the container (`sqlplus` client, no
`tnsnames.ora` entry):

| Connect string form | Works? |
|---|---|
| Easy Connect, service name: `reldex_test/<pwd>@//127.0.0.1:1521/RELDEX` | **Yes** |
| Easy Connect, shorthand SID: `reldex_test/<pwd>@//127.0.0.1:1521:RELDEX` | **No** — `ORA-12545: Connect failed because target host or object does not exist` |
| Full TNS descriptor with `CONNECT_DATA=(SID=RELDEX)` | **Yes** |

In practice: **use the service name (`RELDEX`) with Easy Connect syntax**
(`host:port/service`) for anything driver-facing — it is the simplest form
that actually works on this image. The colon-SID Easy Connect shorthand is
not usable here; if a SID-only connection is ever required, build a full
`CONNECT_DATA=(SID=RELDEX)` descriptor instead.

The test/application user `RELDEX_TEST` is created idempotently on first
start by `init/01_create_test_user.sh`, mounted read-only into the image's
`/opt/oracle/scripts/setup` hook. The image's `runUserScripts.sh` sources
every `*.sh` there on first boot, so the script runs with the container's
environment available.

**Its password comes from `RELDEX_TEST_PWD` in the untracked `.env`**, which
`compose.yaml` passes into the container; no password appears in any tracked
file. The script refuses to run rather than invent one if the variable is
missing, and refuses a value containing a quote, ampersand, semicolon or
whitespace, because it interpolates the value into SQL text.

It is idempotent and safe to re-run against a container that is already up,
which is how to change the test user's password **without destroying the
database**:

```bash
docker exec -e RELDEX_TEST_PWD="<the new value>" reldex-oracle19c \
  bash /opt/oracle/scripts/setup/01_create_test_user.sh
```

(Update `.env` to match, or the next `run-it.sh` will use the old value.)
`ORACLE_PWD` is different: DBCA consumes it on first start only, so changing
it afterwards needs the image's own `/opt/oracle/setPassword.sh`.

### Important: set `NLS_LANG` for Unicode/Thai correctness

The container's OS locale is `POSIX` and `NLS_LANG` is unset by default.
Connecting without `NLS_LANG=AMERICAN_AMERICA.AL32UTF8` caused every
multi-byte UTF-8 character sent to the server to be corrupted into `U+FFFD`
replacement characters during client/server charset conversion (confirmed:
inserting a 12-character Thai string produced `CHAR_LEN=36`, i.e. one
"character" per raw UTF-8 byte). Setting `NLS_LANG=AMERICAN_AMERICA.AL32UTF8`
on the client side fixed this immediately (`CHAR_LEN=12`, `BYTE_LEN=36`,
correct 3-byte UTF-8 sequences in `DUMP()`). **Any driver or client
connecting to this database must declare its client character set as
AL32UTF8** (via `NLS_LANG`, an OCI charset parameter, or the driver's
equivalent) — this is a real finding for Reldex's own Oracle driver, not
just a test-harness quirk.

### Opening sqlplus in the container

```bash
docker exec -it reldex-oracle19c bash -c 'sqlplus / as sysdba'

# or, to test the actual listener path as the test user. The credentials come
# from the container's own environment, so nothing is typed here and nothing
# reaches the host's shell history:
docker exec -it -e NLS_LANG=AMERICAN_AMERICA.AL32UTF8 reldex-oracle19c \
  bash -c 'sqlplus "$RELDEX_TEST_USER/$RELDEX_TEST_PWD@//localhost:1521/RELDEX"'
```

## Stop / start / reset

```bash
docker compose stop        # stop, keep container + data volume
docker compose start       # resume (fast - no DBCA re-run)
docker compose down        # remove container, keep data volume
docker compose down -v     # full reset: removes the named volume too
                            # (next `up` re-runs DBCA from scratch)
```

## Resource usage observed

Measured on this host (Windows 11, Docker Desktop/WSL2 backend, 16 CPUs,
42 GB RAM allocated to the Docker VM):

- Image pull (compressed): **2.99 GB**
- Image on-disk footprint (`docker images` DISK USAGE — larger than the
  pull size because Oracle's install/cleanup layers don't fully dedupe):
  **9.9 GB**
- Data volume (`reldex-oracle19c-data`) after first start: **~2.3–2.5 GB**
- Container RAM at idle (`docker stats --no-stream`), shortly after
  readiness: **~12.1 GiB** (~31% of the 39.17 GiB the container could see)
- Container CPU at idle: **~1%** (spikes heavily during DBCA)

The idle RAM figure is high for a "test" database because `dbca` auto-sizes
`totalMemory` to roughly 40% of *host-visible* memory when more than 8 CPUs
are visible and no container memory limit (`mem_limit` / cgroup limit) is
set — this image predates cgroup v2 awareness (its own scripts try to read
`/sys/fs/cgroup/memory/memory.limit_in_bytes`, which does not exist on this
host's cgroup v2 setup, and silently fall through to host-based sizing).
See **Limitations** for a follow-up.

## Limitations

- **Unmaintained community image.** Last updated in 2021, based on the
  19.3.0.0 base release with no subsequent Release Updates (RUs) or
  security patches applied. Do not use it as a stand-in for a patched,
  supported Oracle installation.
- **Non-CDB only.** No PDB / pluggable-database / service-per-PDB testing
  is possible with this image. If Reldex needs to validate CDB+PDB
  connection or service-name behavior, use the official image instead (see
  below).
- **amd64 only.** No native Apple Silicon / ARM64 image; on ARM hosts this
  runs under emulation (slow, not represented in the numbers above).
- **TCPS/TLS is not configured.** The listener above is plaintext TCP only.
  Adding a TCPS listener (wallet-based) is a follow-up task, tracked
  informally here — see "Suggested follow-ups" below.
- **Memory sizing is host-scaled, not container-bounded** (see above) —
  consider adding `mem_limit`/`deploy.resources.limits.memory` to
  `compose.yaml` plus an explicit SGA/PGA resize if running on a
  memory-constrained host.
- **Local testing only.** Never expose this container's port beyond
  `127.0.0.1`, never reuse its dev password anywhere real, and never point
  it at anything other than disposable test data.
- **History note.** Before 2026-09-19 the test user was created by
  `init/01_create_test_user.sql`, which carried a literal
  `IDENTIFIED BY "<value>"`, and `.env.example` shipped working defaults for
  both passwords. Those files were tracked, so **those values remain in git
  history**. They were throwaway defaults for a database bound to
  `127.0.0.1` and are not used anywhere else, but if this container was ever
  reachable from another host, change `RELDEX_TEST_PWD` in `.env` and re-run
  the setup hook as shown above.

### Official alternative (for later CDB/PDB coverage)

For CDB/PDB-accurate testing later in the project, use Oracle's own image:
`container-registry.oracle.com/database/enterprise:19.3.0.0`. This requires
logging in with an Oracle Single Sign-On (SSO) account at
`container-registry.oracle.com` and accepting Oracle's license terms before
`docker pull`/`docker login` will work. Not set up as part of this task.

## Files

- `compose.yaml` — service definition (loopback-only port, named volume,
  healthcheck, `restart: "no"`, `stop_grace_period: 2m`).
- `.env.example` — placeholder env values; copy to `.env` (gitignored) for
  real local use.
- `init/01_create_test_user.sh` — idempotent `RELDEX_TEST` user/grants, with
  the password taken from the container environment. Auto-run by the image's
  setup hook on first start, and re-runnable by hand at any time.
