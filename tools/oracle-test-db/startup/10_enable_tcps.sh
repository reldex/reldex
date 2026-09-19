#!/bin/bash
# ---------------------------------------------------------------------------
# tools/oracle-test-db/startup/10_enable_tcps.sh
#
# Adds a TCPS (TLS) endpoint to the Phase 0 test database's listener, so spike
# S8 (ADR-0001) can be run against something. Idempotent by construction: the
# image runs everything under `/opt/oracle/scripts/startup` on **every**
# container start (`runOracle.sh` line 190 -> `runUserScripts.sh`), so this
# script has to be safe to run again and again.
#
# It can also be run by hand against a container that is already up:
#
#     docker exec reldex-oracle19c bash /opt/oracle/scripts/startup/10_enable_tcps.sh
#
# What it does, in order:
#
#   1. creates a wallet under the **persisted** volume
#      (`/opt/oracle/oradata/dbconfig/$ORACLE_SID/wallet`, i.e. inside the named
#      volume `reldex-oracle19c-data`) holding a throwaway test CA and a server
#      certificate signed by it, unless one is already there and still valid;
#   2. writes `sqlnet.ora` and `listener.ora` — which are symlinks from
#      `$ORACLE_HOME/network/admin` into the same persisted directory — so the
#      listener offers `(PROTOCOL=TCPS)(PORT=2484)` beside the existing TCP
#      endpoint;
#   3. reloads the listener and waits for the instance to register the service
#      again.
#
# Nothing here is a secret worth protecting: the CA, the server key and the
# wallet password exist only inside this container's volume, for a listener
# bound to `127.0.0.1` on one developer machine. They are still generated
# randomly, kept at mode 600 and never echoed, because a script that prints a
# private key teaches the wrong habit.
#
# `runUserScripts.sh` **sources** `*.sh` files, so this file must not call
# `exit`: everything lives in a function whose status is returned.
# ---------------------------------------------------------------------------

reldex_enable_tcps() {
    local sid="${ORACLE_SID:-RELDEX}"
    local oracle_home="${ORACLE_HOME:-/opt/oracle/product/19c/dbhome_1}"
    local dbconfig="/opt/oracle/oradata/dbconfig/${sid}"
    local wallet="${RELDEX_TCPS_WALLET_DIR:-${dbconfig}/wallet}"
    local tcps_port="${RELDEX_TCPS_PORT:-2484}"
    local tcp_port="${RELDEX_TCP_PORT:-1521}"
    # The names the server certificate is valid for. There is deliberately **no
    # IP address** among them: `oracledb`'s rustls client turns the descriptor's
    # HOST into the name it verifies, so `HOST=127.0.0.1` must fail the name
    # check while `HOST=localhost` succeeds. That difference is what spike S8's
    # hostname-verification test observes. Add `IP:127.0.0.1` here if you would
    # rather have the numeric form work; the S8 test that expects it to fail
    # says so in its own comment.
    local sans="${RELDEX_TCPS_SANS:-DNS:localhost,DNS:reldex-oracle19c}"
    local log
    log() { echo "[tcps] $*"; }

    if [ ! -d "$dbconfig" ]; then
        echo "[tcps] $dbconfig does not exist; is the database created?" >&2
        return 1
    fi

    # -- 1. wallet ----------------------------------------------------------
    if [ -s "${wallet}/cwallet.sso" ] && [ -s "${wallet}/ca.pem" ] \
        && openssl x509 -in "${wallet}/server.crt" -noout -checkend 604800 >/dev/null 2>&1
    then
        log "wallet present and the server certificate is valid for at least a week"
    else
        log "generating a test CA and a server certificate in ${wallet}"
        rm -rf "$wallet"
        mkdir -p "$wallet"
        chmod 700 "$wallet"

        local pwd_file="${wallet}/.wallet-password"
        # Oracle wallet passwords must be at least 8 characters and contain a
        # digit; base64 without the awkward characters satisfies both.
        openssl rand -base64 33 | tr -d '\n/+=' | cut -c1-24 > "$pwd_file"
        printf '9' >> "$pwd_file"
        chmod 600 "$pwd_file"
        local pw
        pw=$(cat "$pwd_file")

        # Three config files rather than one with `-subj` overrides. OpenSSL
        # 1.0.2 (this image) ignores `-subj` when the config names a
        # `distinguished_name` section under `prompt = no`, and the result was
        # a CA and a server certificate carrying the **same** subject DN — an
        # ambiguous chain that Oracle's SSL adapter refuses outright
        # (`TNS-00540`) and that a path builder has no business seeing either.
        #
        # The SAN list is substituted in here rather than read from the
        # environment for a related reason: OpenSSL 1.0.2 expands `${ENV::…}`
        # when it *loads* the file, so an unset variable breaks every command
        # that names the file, including ones that never use that section.
        cat > "${wallet}/ca.cnf" <<'CNF'
[ req ]
distinguished_name = dn
prompt             = no
x509_extensions    = v3_ca

[ dn ]
C  = XX
O  = Reldex Phase 0 test
CN = Reldex Phase 0 local test CA

[ v3_ca ]
basicConstraints     = critical,CA:TRUE,pathlen:0
keyUsage             = critical,keyCertSign,cRLSign
subjectKeyIdentifier = hash
CNF

        cat > "${wallet}/other-ca.cnf" <<'CNF'
[ req ]
distinguished_name = dn
prompt             = no
x509_extensions    = v3_ca

[ dn ]
C  = XX
O  = Reldex Phase 0 test
CN = Reldex Phase 0 unrelated CA

[ v3_ca ]
basicConstraints     = critical,CA:TRUE,pathlen:0
keyUsage             = critical,keyCertSign,cRLSign
subjectKeyIdentifier = hash
CNF

        cat > "${wallet}/server.cnf" <<CNF
[ req ]
distinguished_name = dn
prompt             = no

[ dn ]
C  = XX
O  = Reldex Phase 0 test
CN = localhost

[ v3_server ]
basicConstraints       = critical,CA:FALSE
keyUsage               = critical,digitalSignature,keyEncipherment
extendedKeyUsage       = serverAuth
subjectKeyIdentifier   = hash
authorityKeyIdentifier = keyid,issuer
subjectAltName         = ${sans}
CNF

        # The server's key pair is generated **inside** the Oracle wallet by
        # `orapki`, and only the certificate signing request leaves it. An
        # earlier revision took the shorter route — build a PKCS#12 with
        # `openssl pkcs12 -export` and name it `ewallet.p12` — and the listener
        # refused every handshake with `TNS-12560 / TNS-00540: SSL protocol
        # adapter failure` while the same file verified perfectly under
        # `openssl verify`. 19.3's NZ layer will not serve a third-party
        # PKCS#12; the wallet has to be an orapki wallet. That is worth knowing
        # before spending an afternoon on cipher suites, so it is written down
        # rather than silently worked around.
        local subject="CN=localhost,O=Reldex Phase 0 test,C=XX"
        (
            set -e
            umask 077
            cd "$wallet"
            orapki() { "${oracle_home}/bin/orapki" "$@" -nologo; }

            # The test CA, and an unrelated one that signs nothing. Spike S8
            # hands the second to the client to prove an untrusted issuer is
            # refused rather than waved through; a real CA certificate that
            # simply is not *the* one is a more honest negative than a corrupt
            # file.
            openssl genrsa -out ca.key 3072 2>/dev/null
            openssl req -x509 -new -key ca.key -sha256 -days 3650 \
                -config ca.cnf -out ca.pem 2>/dev/null
            openssl genrsa -out other-ca.key 2048 2>/dev/null
            openssl req -x509 -new -key other-ca.key -sha256 -days 3650 \
                -config other-ca.cnf -out other-ca.pem 2>/dev/null

            # An empty auto-login wallet, then a key pair and a self-signed
            # placeholder inside it, then the request for the real certificate.
            orapki wallet create -wallet . -auto_login -pwd "$pw" >/dev/null
            orapki wallet add -wallet . -dn "$subject" -keysize 2048 \
                -pwd "$pw" >/dev/null
            orapki wallet export -wallet . -dn "$subject" \
                -request server.csr -pwd "$pw" >/dev/null

            # Signed outside the wallet, because `orapki` in 19.3 cannot put a
            # subjectAltName on a certificate and rustls matches **only** the
            # SAN — a common name of `localhost` is invisible to it. serverAuth
            # is required as well: rustls-webpki checks the end-entity
            # certificate's extended key usage.
            openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key \
                -CAcreateserial -sha256 -days 825 \
                -extfile server.cnf -extensions v3_server -out server.crt 2>/dev/null

            # The issuer has to be trusted before its certificate can be
            # imported as the wallet's own, and the auto-login file is rewritten
            # afterwards so the listener sees the finished wallet.
            orapki wallet add -wallet . -trusted_cert -cert ca.pem -pwd "$pw" >/dev/null
            orapki wallet add -wallet . -user_cert -cert server.crt -pwd "$pw" >/dev/null
            orapki wallet create -wallet . -auto_login -pwd "$pw" >/dev/null
        ) || { echo "[tcps] wallet generation failed" >&2; return 1; }

        chmod 600 "${wallet}"/*.key "${wallet}"/ewallet.p12 "${wallet}"/cwallet.sso 2>/dev/null
        chmod 644 "${wallet}/ca.pem" "${wallet}/other-ca.pem" "${wallet}/server.crt"
        log "wallet created"
    fi

    # -- 2. sqlnet.ora and listener.ora -------------------------------------
    # Both are symlinked from $ORACLE_HOME/network/admin into $dbconfig, so the
    # files written here are the ones the listener and the instance read, and
    # they live on the persisted volume rather than in the container layer.
    #
    # SSL_VERSION is pinned to 1.2: 19.3 has no TLS 1.3, and leaving the
    # negotiation open lets the endpoint answer TLS 1.0/1.1 with CBC suites,
    # which rustls will not speak at all — measured, not assumed (see below).
    # The cipher suites are the AEAD intersection of what 19.3 offers and what
    # rustls implements for TLS 1.2; see
    # docs/exec-plans/active/phase-0-spike-results.md, spike S8.
    #
    # **Both files get them, and only one of the two is load-bearing.** The
    # listener reads its own SSL parameters from `listener.ora`: with the pins
    # in `sqlnet.ora` alone, `openssl s_client -tls1_1` still completed a
    # handshake with `ECDHE-RSA-AES256-SHA`, and so did `AES256-SHA`, which is
    # in neither list. With them in `listener.ora` both are refused. The
    # `sqlnet.ora` copy governs clients running **inside** the container
    # (sqlplus over a TCPS descriptor), which is worth having and is not the
    # same thing.
    cat > "${dbconfig}/sqlnet.ora" <<SQLNET
NAME.DIRECTORY_PATH = (TNSNAMES, EZCONNECT, HOSTNAME)

# Written by tools/oracle-test-db/startup/10_enable_tcps.sh. Edit that script.
WALLET_LOCATION =
  (SOURCE =
    (METHOD = FILE)
    (METHOD_DATA = (DIRECTORY = ${wallet}))
  )
SSL_CLIENT_AUTHENTICATION = FALSE
SSL_VERSION = 1.2
SSL_CIPHER_SUITES = (TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384, TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256)
SQLNET

    cat > "${dbconfig}/listener.ora" <<LISTENER
# Written by tools/oracle-test-db/startup/10_enable_tcps.sh. Edit that script.
LISTENER =
(DESCRIPTION_LIST =
  (DESCRIPTION =
    (ADDRESS = (PROTOCOL = IPC)(KEY = EXTPROC1))
    (ADDRESS = (PROTOCOL = TCP)(HOST = 0.0.0.0)(PORT = ${tcp_port}))
    (ADDRESS = (PROTOCOL = TCPS)(HOST = 0.0.0.0)(PORT = ${tcps_port}))
  )
)

WALLET_LOCATION =
  (SOURCE =
    (METHOD = FILE)
    (METHOD_DATA = (DIRECTORY = ${wallet}))
  )
SSL_CLIENT_AUTHENTICATION = FALSE
SSL_VERSION = 1.2
SSL_CIPHER_SUITES = (TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384, TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256)

DEDICATED_THROUGH_BROKER_LISTENER = ON
DIAG_ADR_ENABLED = off
LISTENER

    # -- 3. reload and verify ----------------------------------------------
    # `lsnrctl reload` re-reads the parameter file but does **not** open a
    # listening endpoint that was not there before (measured on this image: the
    # reload succeeds and the endpoint summary is unchanged). So: reload first,
    # and if the TCPS endpoint is still missing, restart the listener. The
    # database itself is untouched either way — PMON re-registers within a few
    # seconds, and `ALTER SYSTEM REGISTER` below does not wait for it.
    "${oracle_home}/bin/lsnrctl" reload >/dev/null 2>&1
    if ! "${oracle_home}/bin/lsnrctl" status 2>/dev/null \
        | grep -q "PROTOCOL=tcps).*PORT=${tcps_port}"
    then
        log "the TCPS endpoint is new, so the listener is being restarted"
        "${oracle_home}/bin/lsnrctl" stop >/dev/null 2>&1
        sleep 2
        "${oracle_home}/bin/lsnrctl" start >/dev/null 2>&1
    fi

    # PMON re-registers the service with the listener a few seconds after a
    # reload. Nudging it is faster and makes the script's own verification
    # meaningful rather than racy.
    "${oracle_home}/bin/sqlplus" -S -L / as sysdba >/dev/null 2>&1 <<'SQL'
ALTER SYSTEM REGISTER;
EXIT
SQL

    local attempt
    for attempt in 1 2 3 4 5 6 7 8 9 10; do
        if "${oracle_home}/bin/lsnrctl" status 2>/dev/null \
            | grep -q "PROTOCOL=tcps).*PORT=${tcps_port}"
        then
            log "listening on TCPS port ${tcps_port}; CA certificate at ${wallet}/ca.pem"
            return 0
        fi
        sleep 2
    done

    echo "[tcps] the listener is not reporting a TCPS endpoint on ${tcps_port}" >&2
    "${oracle_home}/bin/lsnrctl" status 2>&1 | tail -20 >&2
    return 1
}

reldex_enable_tcps
