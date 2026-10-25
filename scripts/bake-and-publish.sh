#!/usr/bin/env bash
# One unattended re-bake, start to finish: plan, bake the planet, upload to R2, switch
# archives.json, purge it from the CDN and delete archives no index refers to any more.
#
# Nothing switches unless the whole bake succeeded. archives.json goes up last, so a run that
# fails anywhere leaves the published index and every archive it names exactly as they were.
#
# Expects an empty directory in DATA and these in the environment:
#   R2_ACCESS_KEY_ID, R2_SECRET_ACCESS_KEY, R2_ACCOUNT_ID   object read/write on the bucket
#   CF_API_TOKEN, CF_ZONE_ID                                cache purge on the serving zone
# Optional:
#   BUCKET (arnis-tiles), PREFIX (v1), PUBLIC_BASE (https://tiles.arnisproject.com)
#   HEALTHCHECK_URL   pinged at start, on success and on failure (healthchecks.io style)
#   SMOKE=1           bake Liechtenstein only, publish it under smoke-test/, then delete that

set -euo pipefail

BIN="${BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release/arnis-tiles}"
DATA="${DATA:?set DATA to an empty directory for the bake}"
BUCKET="${BUCKET:-arnis-tiles}"
PREFIX="${PREFIX:-v1}"
PUBLIC_BASE="${PUBLIC_BASE:-https://tiles.arnisproject.com}"
SMOKE="${SMOKE:-}"
ONLY=()
if [ -n "$SMOKE" ]; then
    PREFIX=smoke-test
    ONLY=(--only liechtenstein)
fi

log() { echo "[$(date -u +%H:%M:%S)] $*"; }

ping_hc() {
    [ -n "${HEALTHCHECK_URL:-}" ] || return 0
    curl -fsS -m 10 --retry 3 -o /dev/null "${HEALTHCHECK_URL}$1" || true
}
trap '[ $? -eq 0 ] || ping_hc /fail' EXIT
ping_hc /start

# rclone reads the remote from the environment, so no config file has to exist.
export RCLONE_CONFIG_BAKE_TYPE=s3
export RCLONE_CONFIG_BAKE_PROVIDER=Cloudflare
export RCLONE_CONFIG_BAKE_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID"
export RCLONE_CONFIG_BAKE_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY"
export RCLONE_CONFIG_BAKE_ENDPOINT="https://${R2_ACCOUNT_ID}.r2.cloudflarestorage.com"
# A token scoped to one bucket cannot call CreateBucket, which rclone tries by default.
export RCLONE_CONFIG_BAKE_NO_CHECK_BUCKET=true
export RCLONE_CONFIG_BAKE_CHUNK_SIZE=64M
export RCLONE_CONFIG_BAKE_UPLOAD_CONCURRENCY=8
REMOTE="bake:$BUCKET/$PREFIX"

mkdir -p "$DATA"
if [ -n "$(ls -A "$DATA")" ]; then
    echo "$DATA is not empty; the bake has to start from nothing" >&2
    exit 1
fi
CACHE="$DATA/cache" WORK="$DATA/work" OUT="$DATA/out"
tiles() { "$BIN" --cache "$CACHE" --work "$WORK" --out "$OUT" "$@"; }
df -h "$DATA"

# The index clients have now. Its archives must survive this run: clients cache it for a day.
# A full run insists on reading it, because without it the cleanup below would delete them.
PREVIOUS="$DATA/previous-archives.json"
if [ -n "$SMOKE" ]; then
    echo '{"archives":[]}' > "$PREVIOUS"
else
    rclone copyto "$REMOTE/archives.json" "$PREVIOUS"
    jq -e '.archives | length > 0' "$PREVIOUS" > /dev/null
fi

log "plan"
# The plan reads openstreetmap.fr as well as Geofabrik. Either being down for an hour should
# delay a scheduled run, not cancel it until next month.
for wait in 300 600 1200 2400 4800 0; do
    tiles plan && break
    [ "$wait" -gt 0 ] || exit 1
    log "plan failed; retrying in ${wait}s"
    sleep "$wait"
done
log "run"
tiles run ${ONLY[@]+"${ONLY[@]}"}
tiles finalize
INDEX="$OUT/archives.json"

# Refuse an index that would serve less than the one it replaces. Overlays are left out of the
# continent check because a full bake plans their regions itself.
if [ -z "$SMOKE" ]; then
    missing=$(jq -rn --slurpfile p "$PREVIOUS" --slurpfile n "$INDEX" \
        '($p[0].archives | map(.name) | map(select(startswith("overlay") | not)))
         - ($n[0].archives | map(.name)) | join(" ")')
    if [ -n "$missing" ]; then
        echo "the new index has no archive for: $missing" >&2
        exit 1
    fi
    before=$(jq '[.archives[].tiles] | add // 0' "$PREVIOUS")
    after=$(jq '[.archives[].tiles] | add // 0' "$INDEX")
    log "tiles: $before published, $after baked"
    if [ "$((after * 10))" -lt "$((before * 9))" ]; then
        echo "the new bake has under 90% of the published tiles" >&2
        exit 1
    fi
fi

log "upload"
for f in $(jq -r '.archives[].file' "$INDEX"); do
    [ -f "$OUT/$f" ] || { echo "the index names $f, which is not in $OUT" >&2; exit 1; }
done
rclone copy "$OUT" "$REMOTE" --include "*.pmtiles" --transfers 4 --stats 1m --stats-one-line
rclone check "$OUT" "$REMOTE" --include "*.pmtiles" --size-only --one-way
rclone copyto "$INDEX" "$REMOTE/archives.json"
log "switched $PREFIX/archives.json"

# Until this purge the edge keeps answering with the old index.
purge=$(curl -fsS -X POST "https://api.cloudflare.com/client/v4/zones/${CF_ZONE_ID}/purge_cache" \
    -H "Authorization: Bearer ${CF_API_TOKEN}" -H "Content-Type: application/json" \
    --data "{\"files\":[\"${PUBLIC_BASE}/${PREFIX}/archives.json\"]}")
[ "$(jq -r .success <<<"$purge")" = true ] || { echo "cache purge failed: $purge" >&2; exit 1; }
log "purged ${PUBLIC_BASE}/${PREFIX}/archives.json"

if [ -n "$SMOKE" ]; then
    rclone lsl "$REMOTE"
    rclone delete "$REMOTE"
    log "smoke test passed; removed $PREFIX/"
    ping_hc ""
    exit 0
fi

# Anything neither index names is at least one bake old, far past the day clients cache for.
jq -r '.archives[].file' "$INDEX" "$PREVIOUS" | sort -u > "$DATA/keep"
rclone lsf "$REMOTE" --files-only --include "*.pmtiles" | sort > "$DATA/published"
for f in $(comm -23 "$DATA/published" "$DATA/keep"); do
    log "delete $f"
    rclone deletefile "$REMOTE/$f"
done

log "done"
ping_hc ""
