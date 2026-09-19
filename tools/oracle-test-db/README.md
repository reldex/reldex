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

### What `run-it.ps1` / `run-it.sh` put in the environment

Nothing is written to a file, nothing appears on a command line, and the
PowerShell runner clears the password variables again in its `finally` block.

| Variable | From | Used by |
|---|---|---|
| `RELDEX_TEST_ORACLE_DSN` / `_USER` / `_PASSWORD` | `RELDEX_TEST_PWD` | every spike |
| `RELDEX_TEST_ORACLE_SYSTEM_USER` / `_SYSTEM_PASSWORD` | `ORACLE_PWD`, as `SYSTEM` | S4's privileged-cancel candidate only |
| `RELDEX_TEST_ORACLE_SYSDBA_USER` / `_SYSDBA_PASSWORD` | `ORACLE_PWD`, as `SYS` | S13 (`AS SYSDBA` over the listener) only |
| `RELDEX_TEST_ORACLE_TCPS_DSN` / `_TCPS_CA_DIR` / `_TCPS_WRONG_CA_DIR` | the exported CA PEMs, when present | S8, the U-14 canary |

### Upstream canaries

Besides the spikes, the driver crate carries a canary suite that asserts each
upstream `oracledb` defect is **still present**, so a fix upstream arrives as a
failing test naming the guard it makes removable. Run it whenever the pinned
`oracledb` version moves:

```powershell
# no database needed; also runs in `cargo test --workspace`
cargo test -p reldex-driver-oracle-thin --test canary_upstream_offline

# the live half, single-threaded (it spawns child processes that abort on purpose)
tools\oracle-test-db\run-it.ps1 canary_upstream_live -- --test-threads=1
```

The procedure is `docs/exec-plans/active/oracledb-upgrade-checklist.md`.

Every test that needs one of the optional pairs **skips itself and says so**
when the variables are absent, so an ordinary run never requires a DBA
password. `AS SYSDBA` over the listener works on this image because
`remote_login_passwordfile = EXCLUSIVE` (confirmed by S13); with `NONE` it
could not, and the S13 tests would report that as a database-configuration
finding rather than a driver failure.

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

## TCPS (TLS) listener — ADR-0001 spike S8

A second endpoint, `127.0.0.1:2484`, speaks TCPS. It is set up by
`startup/10_enable_tcps.sh`, which `compose.yaml` mounts into the image's
`/opt/oracle/scripts/startup` hook — the hook the image runs on **every**
start, unlike `setup`, which runs once after the database is created. The
script is idempotent and safe to run by hand against a live container:

```bash
docker exec reldex-oracle19c bash /opt/oracle/scripts/startup/10_enable_tcps.sh
```

It creates, inside the **persisted volume** at
`/opt/oracle/oradata/dbconfig/RELDEX/wallet`:

| File | What it is |
|---|---|
| `cwallet.sso`, `ewallet.p12` | the auto-login Oracle wallet the listener opens at start-up |
| `ca.pem` | the test CA's certificate — **public**, and the one the client needs |
| `other-ca.pem` | an unrelated CA that signed nothing, for the negative test |
| `ca.key`, `server.key` | private keys; mode 600, inside the volume, never on the host |
| `.wallet-password` | a random wallet password generated at setup time, mode 600 |

Nothing there is a secret worth protecting — it belongs to a listener bound to
`127.0.0.1` on one developer machine — but it is generated randomly and kept out
of git all the same. `.gitignore` already covers `wallet/`, `*.pem`, `*.key`,
`*.p12`, `cwallet.sso` and `ewallet.p12`.

It also rewrites `listener.ora` and `sqlnet.ora`. Those are symlinks from
`$ORACLE_HOME/network/admin` into the same persisted directory, so the
configuration survives a container re-creation; re-creating the container is
therefore safe, and `docker compose up -d` brings TCPS back by itself.

### Getting the CA certificate to the client

`oracledb`'s rustls client reads exactly one file: **`ewallet.pem`** in the
directory it is given. It is **not** an Oracle wallet — it is a PEM bundle, and
when it contains no private key every certificate in it is added to the trusted
roots. So the client-side "wallet" here is a directory holding a copy of
`ca.pem` named `ewallet.pem`:

```bash
mkdir -p tools/oracle-test-db/wallet tools/oracle-test-db/wallet-untrusted
docker exec reldex-oracle19c bash -lc \
  'cat /opt/oracle/oradata/dbconfig/RELDEX/wallet/ca.pem' \
  > tools/oracle-test-db/wallet/ewallet.pem
docker exec reldex-oracle19c bash -lc \
  'cat /opt/oracle/oradata/dbconfig/RELDEX/wallet/other-ca.pem' \
  > tools/oracle-test-db/wallet-untrusted/ewallet.pem
```

`run-it.ps1` / `run-it.sh` look for those two files and, when they are there,
export `RELDEX_TEST_ORACLE_TCPS_DSN`, `RELDEX_TEST_ORACLE_TCPS_CA_DIR` and
`RELDEX_TEST_ORACLE_TCPS_WRONG_CA_DIR`. Without them the S8 tests skip
themselves and say why.

### `localhost`, not `127.0.0.1`

The server certificate carries `subjectAltName = DNS:localhost,
DNS:reldex-oracle19c` and **no IP address**, so the connect string has to be
`tcps://localhost:2484/RELDEX`. rustls ignores the common name entirely and
matches only the SAN, against whatever the descriptor's `HOST` says — there is
no `SSL_SERVER_DN_MATCH=OFF` in this client to fall back on. The numeric form
therefore fails with `certificate not valid for name "127.0.0.1"`, which is
exactly what `s8_tcps.rs`'s host-name test asserts.

Two consequences worth knowing:

- Adding `IP:127.0.0.1` to `RELDEX_TCPS_SANS` (an environment variable the
  script honours) would make the numeric form work — and would break that test
  on purpose. Change both together.
- Resolving `localhost` costs about **2 seconds per connect** on this Windows
  host, roughly twenty times a whole plaintext connect. That is name resolution,
  not TLS: the same 2 s appears on a plain `localhost:1521/RELDEX`. The S8
  latency test measures TCP and TCPS against the *same* host name so the TLS
  figure is not contaminated by it.

### Verifying the server side without the driver

```bash
# negotiated protocol and cipher, chain verified against the test CA
docker exec reldex-oracle19c bash -lc \
  'echo | openssl s_client -connect localhost:2484 \
     -CAfile /opt/oracle/oradata/dbconfig/RELDEX/wallet/ca.pem 2>&1 \
   | grep -E "Protocol  :|Cipher    :|Verify return"'
# ->     Protocol  : TLSv1.2
#        Cipher    : ECDHE-RSA-AES256-GCM-SHA384
#        Verify return code: 0 (ok)

# the endpoint is registered and the service is on it
docker exec reldex-oracle19c lsnrctl status
```

### Troubleshooting

| Symptom | Cause |
|---|---|
| `TNS-12560 / TNS-00540: SSL protocol adapter failure` in `listener.log`, while `openssl verify` is happy | The wallet is not an `orapki` wallet. 19.3 will **not** serve a PKCS#12 produced by `openssl pkcs12 -export`, however valid it is. The script generates the key pair inside the wallet with `orapki wallet add` and imports only the signed certificate; do not "simplify" that back. |
| The same failure with both certificates named `CN=placeholder` | OpenSSL 1.0.2 (this image) ignores `-subj` when the config file names a `distinguished_name` section under `prompt = no`. The script uses one config file per certificate instead. |
| `lsnrctl reload` reports success but the endpoint summary has no TCPS line | `reload` does not open a listening endpoint that was not there before. The script falls back to `lsnrctl stop` + `start`, which does. |
| `openssl s_client -tls1_1` still completes a handshake | `SSL_VERSION` / `SSL_CIPHER_SUITES` were set in `sqlnet.ora` only. The **listener** reads its own from `listener.ora`; the script writes both. |
| `invalid peer certificate: UnknownIssuer` from the driver | The client's `ewallet.pem` is missing, empty, or the wrong CA. Re-export it as above. |
| `The listener supports no services` | PMON has not re-registered yet. Wait a few seconds, or `ALTER SYSTEM REGISTER;` as SYSDBA. |

The listener's own log is at
`/opt/oracle/diag/tnslsnr/<hostname>/listener/trace/listener.log`.

## Memory footprint

`dbca` sized this instance from **host** memory when it was created, which on a
39 GiB machine meant `sga_target` 11.75 GiB and `pga_aggregate_target` 3.92 GiB
— about 15.7 GiB of configured memory for a test database that never needs it.
The pages are committed lazily, so `docker stats` showed far less at rest, but
nothing bounded it.

It is now capped from both sides, **without re-creating the database**:

| | Before | After |
|---|---|---|
| `sga_target` / `sga_max_size` | 11.75 GiB | **1536 MB** |
| `pga_aggregate_target` | 3.92 GiB | **512 MB** |
| `pga_aggregate_limit` | 7.83 GiB | **2048 MB** (its minimum) |
| `processes` | 1280 | **300** |
| Container ceiling | none | **`mem_limit: 4g`** |
| `docker stats` at rest | 693.5 MiB / 39.17 GiB (1.73%) | **1.83 GiB / 4 GiB (45.9%)** |

The "after" number is larger than the "before" one because it was taken after a
full integration run had touched the SGA, not because the instance grew: the
figure that changed is the 15.7 GiB it was entitled to ask for.

`pga_aggregate_limit` has a hard floor of **2048 MB** and must also be at least
`3 MB × processes`, which is why `processes` came down to 300 — at 1280 the floor
would have been 3.75 GiB. Setting it to 1536 MB is what made the instance refuse
to start with `ORA-00093` on the first attempt.

A pfile of the previous settings is kept **on the volume** at
`/opt/oracle/oradata/dbconfig/RELDEX/pfileRELDEX.ora.before-memcap`. To go back:

```bash
docker exec reldex-oracle19c bash -lc \
  'sqlplus -S -L / as sysdba <<EOF
CREATE SPFILE FROM PFILE=''/opt/oracle/oradata/dbconfig/RELDEX/pfileRELDEX.ora.before-memcap'';
EOF'
# then raise or remove mem_limit in compose.yaml and `docker compose up -d`
```

If the instance ever refuses to start after a parameter change, the container
exits and `sqlplus` inside it is unreachable. Repair the spfile from a throwaway
container on the same volume:

```bash
docker run --rm -v reldex-oracle19c-data:/opt/oracle/oradata \
  --entrypoint bash doctorkirk/oracle-19c:19.3 -lc '
    export ORACLE_SID=RELDEX
    S=/opt/oracle/oradata/dbconfig/RELDEX/spfileRELDEX.ora
    $ORACLE_HOME/bin/sqlplus -S -L / as sysdba <<EOF
CREATE PFILE=''/tmp/p.ora'' FROM SPFILE=''$S'';
EOF
    # edit /tmp/p.ora, then:
    $ORACLE_HOME/bin/sqlplus -S -L / as sysdba <<EOF
CREATE SPFILE=''$S'' FROM PFILE=''/tmp/p.ora'';
EOF'
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
- ~~**TCPS/TLS is not configured.**~~ **Done** — see "TCPS (TLS) listener"
  above. What remains untested is **mutual** TLS: the listener runs with
  `SSL_CLIENT_AUTHENTICATION = FALSE`, because `oracledb` cannot both trust a
  private CA and present a client certificate (one `ewallet.pem` is read as one
  or the other).
- ~~**Memory sizing is host-scaled, not container-bounded.**~~ **Done** — see
  "Memory footprint" above.
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
- `startup/10_enable_tcps.sh` — idempotent TCPS wallet + listener setup, run by
  the image's **startup** hook on every container start. See "TCPS (TLS)
  listener" above.
- `wallet/`, `wallet-untrusted/` — untracked, created by the export step in
  "Getting the CA certificate to the client". Each holds an `ewallet.pem` that
  the driver reads as a set of trusted roots.
- `run-it.ps1`, `run-it.sh` — load `.env` and run the opt-in integration tests
  with the connection settings in the environment.
