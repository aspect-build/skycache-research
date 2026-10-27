#!/usr/bin/env bash
# Bazel credential helper that answers every request with SKYCACHE_TOKEN as a bearer.
#
# The token must be a JWT signed by an issuer in SKYCACHE_OIDC_ISSUERS. It is read from the
# environment per call and never written to disk.
set -o errexit -o nounset -o pipefail

token=${SKYCACHE_TOKEN:?set to a JWT from an issuer skycache trusts}

# Consume Bazel's request so it does not see a broken pipe; every URI gets the same token.
cat >/dev/null

printf '{"headers":{"Authorization":["Bearer %s"]}}\n' "$token"
