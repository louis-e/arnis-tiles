# arnis-tiles

Bakes the OpenStreetMap planet into the **Arnis tile archive**: a set of PMTiles files on static
hosting that [Arnis](https://github.com/louis-e/arnis) reads over HTTP range requests, instead of
querying a public Overpass API.

One archive per continent, one z13 tile per ~4.9 km square, one zstd-compressed `AOT1` payload
per tile. A city-sized generation reads a few hundred kilobytes and touches no OSM server.

## Why

Arnis was banned from `overpass-api.de` for using more than its share
([#1347](https://github.com/louis-e/arnis/issues/1347)). Their recommendation was to bake planet
dumps into a format that serves our users. This is that.

Measured against the Overpass path, same bbox in Andorra:

| | Overpass | tile archive |
|---|---|---|
| OSM fetch | 10.0 s | **0.046 s** |
| bytes | 14.9 MB (a comparable Hamburg bbox) | 0.2 MB |
| generated world | 20 regions, 84,107,264 bytes | 20 regions, 84,164,608 bytes |

The 0.07% difference is the ~1.1 m coordinate quantisation and the untagged non-member ways the
archive drops.

## What is in a tile

Everything Arnis renders from, and nothing else:

* ways whose tags match the key list in `src/tags.rs` (which mirrors the Overpass query in
  `retrieve_data.rs`), with geometry inlined
* ways that are members of a kept relation, tagged or not
* nodes carrying one of those keys (POIs - benches, waste baskets, entrances)
* relations with those keys or `type=multipolygon` / `type=building`
* every tag on them except what `osm_parser.rs` already discards (names, `addr:*` bar the house
  number, wikipedia, operator, website, opening hours, source)

So `building:levels`, `height`, `roof:shape`, `roof:material`, `building:colour` and
`start_date` are all there.

**Not** in a tile: OSM node ids for way vertices (the decoder mints them from the coordinate -
this is most of the size saving), element metadata (version, timestamp, changeset, user - which
Geofabrik strips anyway), coastline/ocean/tidal features (Arnis resolves those from satellite
land cover), and coordinate precision below ~1.1 m.

## Requirements

* Rust stable
* **~90 GB free disk** at peak, **~8 GB free RAM** (the largest planned extract is ~2.3 GB and
  its node table dominates)
* a connection you are happy to pull ~86 GB over

## Running a full bake

```sh
cargo build --release

# 1. Decide which extracts cover the world. Writes work/plan.json.
#    First run HEADs all 555 Geofabrik extracts for their sizes and range-reads the headers of
#    the handful with no cutting polygon; both are cached under cache/.
./target/release/arnis-tiles plan

# 2. Download, bake and delete each extract in turn. Resumable - rerun after any interruption
#    and it skips what is already in the store.
./target/release/arnis-tiles run

# 3. Merge into out/<continent>.pmtiles + out/archives.json
./target/release/arnis-tiles finalize
```

Expect roughly:

| | |
|---|---|
| extracts | ~305, ~86 GB downloaded |
| CPU | ~2 h (single-threaded; downloads overlap with baking) |
| wall clock | ~2–4 h depending on bandwidth |
| `work/chunks.db` | ~45 GB, deletable after `finalize` |
| `out/` | ~30–40 GB, this is what you publish |

Check on it any time with `arnis-tiles status`, and look inside a single tile with
`arnis-tiles inspect <x> <y>`.

### A partial bake is useful

`run --only europe-ish-region-ids` bakes a subset, and `finalize` publishes whatever is in the
store. Arnis falls back to Overpass for any tile the archive does not have, so you can ship one
continent at a time rather than waiting for the planet.

## Publishing to Cloudflare R2

R2 is Cloudflare's S3-compatible object storage. It is the right host here because **egress is
free** - the archive is served to every Arnis user at no bandwidth cost.

**Cost at this size:** ~35 GB storage is **$0.38/month** (first 10 GB free, then $0.015/GB).
Uploading the archive is a few thousand multipart PUTs, well under a dollar. User reads land in
Class B ops, whose free tier is 10M/month - far more than Arnis generates. Budget **under
$1/month**.

### One-time setup

1. Sign up at <https://dash.cloudflare.com> (a free account is enough; R2 asks for a card but
   the free tier covers this).
2. **R2 → Create bucket**, name it `arnis-tiles`, pick a location hint near your users.
3. **R2 → Manage API tokens → Create API token**, scope *Object Read & Write* on that bucket.
   Note the Access Key ID, Secret Access Key, and your Account ID.

### Upload

`rclone` handles multipart and resume; `aws s3` works too.

```sh
# ~/.config/rclone/rclone.conf
[r2]
type = s3
provider = Cloudflare
access_key_id = <access key>
secret_access_key = <secret>
endpoint = https://<account id>.r2.cloudflarestorage.com
acl = private

# upload, then verify
rclone copy out/ r2:arnis-tiles/v1/ --progress --transfers 4
rclone ls r2:arnis-tiles/v1/
```

Publish under a version prefix (`v1/`, or a date) so a re-bake can be uploaded and switched to
atomically instead of overwriting the archive users are reading mid-generation.

### Serving it

**R2 → your bucket → Settings → Public access → Custom domain**, and point e.g.
`osm.arnismc.com` at it. Cloudflare then serves the archive through its CDN, and the usual
Cloudflare controls apply: cache rules, rate limiting, WAF, per-URL analytics. Egress stays free.

Avoid the `r2.dev` managed subdomain for production - it is deliberately rate limited.

Then point Arnis at it:

```sh
arnis --osm-tiles-url https://osm.arnismc.com/v1 --bbox "42.500,1.510,42.515,1.535" ...
```

Arnis fetches `archives.json` from that base URL (cached for a day), opens only the archives
whose bounds overlap the bbox, and falls back to Overpass on any error.

## Keeping it fresh

A tile is as fresh as the extract it was baked from - Geofabrik rebuilds daily, so a weekly
re-bake keeps the archive within a week of live OSM. Re-run `plan` (the size cache expires
naturally when you delete `cache/sizes.json`), then `run` against an empty `work/chunks.db`, then
`finalize`.

Published archives carry the date they were built (`europe-20260921.pmtiles`) and are never
overwritten, so `archives.json` is the only file that changes. That is what makes a refresh safe
to publish in place: clients key their range cache on the filename, so a new bake invalidates it
by itself and no Arnis release is needed. Upload the new archives first and `archives.json` last,
then purge it from the CDN cache - until that purge the edge keeps serving the old index, which
is harmless but means nobody sees the new data.

Clients cache the index for 24h, so leave the superseded archive in place for a day before
deleting it. `finalize` prints which file that is.

A re-bake writes coverage cells itself. Only an archive built before cells existed needs
`arnis-tiles cells`, which rescans the files in `out/` (directories only, a second or so) and
writes them into the index.

## Testing locally without uploading

PMTiles needs HTTP range requests, which `python3 -m http.server` does not implement. Any range
capable static server works; the repo's `out/` can be served with one of the many one-file range
servers, then:

```sh
arnis --osm-tiles-url http://127.0.0.1:8787 --bbox "..." --output-dir /tmp/w
```

## Licence

The tool is Apache-2.0. **The archives it produces are a derivative database of OpenStreetMap
and must be published under ODbL 1.0 with attribution to "© OpenStreetMap contributors".** Put
that in `archives.json`'s neighbourhood and in whatever UI surfaces the data.
