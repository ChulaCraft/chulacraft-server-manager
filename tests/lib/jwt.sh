#!/bin/bash
# Mint EdDSA tokens like chulacraft-web does, with openssl only. Source this file.
#   gen_keys DIR            writes DIR/key.pem, sets MCSV_JWT_PUBLIC_KEY (one-line base64 body)
#   mint_token DIR PERM     prints a 60 s token carrying permission PERM

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

gen_keys() {
    openssl genpkey -algorithm ed25519 -out "$1/key.pem" 2> /dev/null
    MCSV_JWT_PUBLIC_KEY="$(openssl pkey -in "$1/key.pem" -pubout -outform DER | openssl base64 -A)"
    export MCSV_JWT_PUBLIC_KEY
}

mint_token() {
    local dir="$1" perm="$2" now head body sig
    now="$(date +%s)"
    head="$(printf '%s' '{"alg":"EdDSA","typ":"JWT"}' | b64url)"
    body="$(printf '{"iss":"chulacraft-web","aud":"mcsv-manager","sub":"test","jti":"t-%s","iat":%s,"exp":%s,"permission":%s}' \
        "$now" "$now" "$((now + 60))" "$perm" | b64url)"
    # pkeyutl -rawin needs a file, not a pipe, to know the message size.
    printf '%s' "$head.$body" > "$dir/msg.tmp"
    sig="$(openssl pkeyutl -sign -inkey "$dir/key.pem" -rawin -in "$dir/msg.tmp" | b64url)"
    printf '%s.%s' "$head.$body" "$sig"
}
