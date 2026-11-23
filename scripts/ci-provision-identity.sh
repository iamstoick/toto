#!/usr/bin/env bash
# CI/local provisioning for Wave 3 identity interop (LDAP + Keycloak).
# All secrets come from the environment (never committed); fixtures
# in the repo carry placeholders only. Idempotent-ish: safe to
# re-run against a fresh ephemeral service.
set -eu

: "${LDAP_HOST:=127.0.0.1}"
: "${LDAP_PORT:=389}"
: "${LDAP_ADMIN_DN:=cn=admin,dc=toto,dc=test}"
: "${LDAP_ADMIN_PW:?set LDAP_ADMIN_PW}"
: "${LDAP_TEST_USER_PW:?set LDAP_TEST_USER_PW}"
: "${LDAP_TEST_USER_PW2:?set LDAP_TEST_USER_PW2}"
: "${KC_BASE:?set KC_BASE (e.g. http://127.0.0.1:8080)}"
: "${KC_ADMIN_USER:=admin}"
: "${KC_ADMIN_PW:?set KC_ADMIN_PW}"
: "${KC_REALM:=toto}"
: "${KC_CLIENT:=refsvc}"
: "${KC_REDIRECT:=http://127.0.0.1:18080/cb}"
: "${KC_USER:=alice}"
: "${KC_USER_PW:?set KC_USER_PW}"
: "${KC_CLIENT_SECRET:=toto-test-client-secret-0123456789ab}"
: "${KC_USER_FIRST:=Alice}"
: "${KC_USER_LAST:=Liddell}"
: "${KC_USER_EMAIL:=alice@toto.test}"
: "${VAULT_PORT:=8200}"
: "${VAULT_TOKEN:?set VAULT_TOKEN (dev root token, test-only)}"

echo "--- Keycloak: start container (start-dev, like the local proof)"
echo "--- (service containers cannot pass the start command; without it the image exits)"
docker run -d --name w3keycloak -p 8080:8080 \
  -e KC_BOOTSTRAP_ADMIN_USERNAME="$KC_ADMIN_USER" \
  -e KC_BOOTSTRAP_ADMIN_PASSWORD="$KC_ADMIN_PW" \
  quay.io/keycloak/keycloak:26.0 start-dev > /dev/null

echo "--- wait for services (LDAP :$LDAP_PORT, Keycloak $KC_BASE)"
for i in $(seq 1 60); do
    if (echo > /dev/tcp/127.0.0.1/"$LDAP_PORT") 2>/dev/null && curl -sf -o /dev/null "$KC_BASE/realms/master/.well-known/openid-configuration"; then
        echo "services ready after ${i}x5s"
        break
    fi
    if [ "$i" = 60 ]; then
        (echo > /dev/tcp/127.0.0.1/"$LDAP_PORT") 2>/dev/null && echo "ldap up :$LDAP_PORT" || echo "ldap DOWN :$LDAP_PORT"
        curl -sf -o /dev/null "$KC_BASE/realms/master/.well-known/openid-configuration" && echo "keycloak up $KC_BASE" || echo "keycloak DOWN $KC_BASE"
        echo "services never became ready" >&2; exit 1
    fi
    sleep 5
done

echo "--- LDAP: seed directory"
export LDAP_TEST_USER_PW LDAP_TEST_USER_PW2
python3 - "$LDAP_TEST_USER_PW" "$LDAP_TEST_USER_PW2" <<'PYEOF' > /tmp/w3-seed.ldif
import sys
t = open('tests/fixtures/ldap/seed.ldif.tmpl').read()
t = t.replace('${LDAP_TEST_USER_PW}', sys.argv[1]).replace('${LDAP_TEST_USER_PW2}', sys.argv[2])
sys.stdout.write(t)
PYEOF
ldapadd -x -H "ldap://$LDAP_HOST:$LDAP_PORT" -D "$LDAP_ADMIN_DN" -w "$LDAP_ADMIN_PW" -c -f /tmp/w3-seed.ldif 2>&1 | tail -n 3

echo "--- Keycloak: admin token"
KC_TOKEN=$(curl -s -X POST "$KC_BASE/realms/master/protocol/openid-connect/token" \
  -d "username=$KC_ADMIN_USER" -d "password=$KC_ADMIN_PW" \
  -d "grant_type=password" -d "client_id=admin-cli" | python3 -c "import sys,json; print(json.load(sys.stdin)['access_token'])")

echo "--- Keycloak: realm $KC_REALM"
curl -s -o /dev/null -w "realm=%{http_code}\n" -X POST "$KC_BASE/admin/realms" \
  -H "Authorization: Bearer $KC_TOKEN" -H "Content-Type: application/json" \
  -d "{\"realm\":\"$KC_REALM\",\"enabled\":true}"

echo "--- Keycloak: client $KC_CLIENT (default RS256)"
curl -s -o /dev/null -w "client=%{http_code}\n" -X POST "$KC_BASE/admin/realms/$KC_REALM/clients" \
  -H "Authorization: Bearer $KC_TOKEN" -H "Content-Type: application/json" \
  -d "{\"clientId\":\"$KC_CLIENT\",\"enabled\":true,\"publicClient\":false,\"directAccessGrantsEnabled\":true,\"standardFlowEnabled\":true,\"redirectUris\":[\"$KC_REDIRECT\"]}"

echo "--- Keycloak: user $KC_USER"
curl -s -o /dev/null -w "user=%{http_code}\n" -X POST "$KC_BASE/admin/realms/$KC_REALM/users" \
  -H "Authorization: Bearer $KC_TOKEN" -H "Content-Type: application/json" \
  -d "{\"username\":\"$KC_USER\",\"enabled\":true,\"email\":\"$KC_USER_EMAIL\",\"emailVerified\":true,\"firstName\":\"$KC_USER_FIRST\",\"lastName\":\"$KC_USER_LAST\",\"credentials\":[{\"type\":\"password\",\"value\":\"$KC_USER_PW\",\"temporary\":false}]}"
UID=$(curl -s "$KC_BASE/admin/realms/$KC_REALM/users?username=$KC_USER" -H "Authorization: Bearer $KC_TOKEN" | python3 -c "import sys,json; print(json.load(sys.stdin)[0]['id'])")
echo "uid=$UID"
CID2=$(curl -s "$KC_BASE/admin/realms/$KC_REALM/clients?clientId=$KC_CLIENT" -H "Authorization: Bearer $KC_TOKEN" | python3 -c "import sys,json; print(json.load(sys.stdin)[0]['id'])")
# Deterministic test-only secret (same class as the test-only DB
# passwords in CI env — no production value, never rotated).
curl -s -o /dev/null -w "secret=%{http_code}
" -X PUT "$KC_BASE/admin/realms/$KC_REALM/clients/$CID2/client-secret" -H "Authorization: Bearer $KC_TOKEN" -H "Content-Type: application/json" -d "{\"value\":\"$KC_CLIENT_SECRET\"}"
echo "--- Vault: dev server on :$VAULT_PORT"
docker run -d --name w3vault -p "$VAULT_PORT:8200" hashicorp/vault:1.15 server -dev -dev-listen-address=0.0.0.0:8200 -dev-root-token-id="$VAULT_TOKEN" > /dev/null
for i in $(seq 1 30); do
    if curl -sf -o /dev/null "http://127.0.0.1:$VAULT_PORT/v1/sys/health"; then
        echo "vault ready after ${i}x2s"
        break
    fi
    if [ "$i" = 30 ]; then echo "vault never became ready" >&2; exit 1; fi
    sleep 2
done
echo "--- Vault: seed placeholder keys (KV-v2 secret/)"
curl -s -o /dev/null -w "ci-only=%{http_code}\n" -X POST "http://127.0.0.1:$VAULT_PORT/v1/secret/data/ci-only" \
  -H "X-Vault-Token: $VAULT_TOKEN" -H "Content-Type: application/json" \
  -d '{"data":{"value":"placeholder-one"}}'
curl -s -o /dev/null -w "ci-cascade=%{http_code}\n" -X POST "http://127.0.0.1:$VAULT_PORT/v1/secret/data/ci-cascade" \
  -H "X-Vault-Token: $VAULT_TOKEN" -H "Content-Type: application/json" \
  -d '{"data":{"value":"cascade-one"}}'
echo "--- provisioned"
