# arnis-tiles

Bakes the OpenStreetMap planet into the Arnis tile archive: a set of PMTiles files on static
hosting that [Arnis](https://github.com/louis-e/arnis) reads over HTTP range requests instead of
querying a public Overpass API.

One archive per continent, one z13 tile per ~4.9 km square, one zstd-compressed `AOT2` payload
per tile. A city-sized generation reads a few hundred kilobytes and touches no OSM server.

## Why

Arnis was banned from `overpass-api.de` for using more than its share
([#1347](https://github.com/louis-e/arnis/issues/1347)). The suggestion from the Overpass side
was to bake planet dumps into something that serves our users directly. This is that.

Same bbox in Andorra (`42.500,1.510,42.515,1.535`), measured on the first published archive:

| | Overpass | tile archive |
|---|---|---|
| OSM fetch | 1.74 MB over the wire | 0.29 MB, one tile |
| requests | 1 | 2 for the index, then 1 per tile |

Arnis renders deterministically, so the two paths can be compared pixel by pixel on the map
preview (`--map-preview`). On Lake Zurich (`47.362,8.538,47.368,8.548`) the first format, AOT1,
differed from Overpass in 2.2% of pixels: a ragged shoreline, because the lake's multipolygon
arrived with most members missing, and one-block jitter from storing 1e-6 degrees. AOT2 differs
in 0.02%, which is the week between the two data snapshots.

## What is in a tile

Everything Arnis renders from, and nothing else:

* every tagged way, minus the values Arnis explicitly discards (see `src/tags.rs`), with its
  geometry inlined
* ways that are members of a kept relation, tagged or not
* nodes carrying a rendered key (benches, waste baskets, entrances and so on)
* relations with those keys, or `type=multipolygon` / `type=building`
* every relation that spans more than one tile, a second time and whole, with all its member
  ways, at tile id `RELATION_TILE_BASE + relation id`. A tile alone only holds the members that
  touch it; Arnis fetches the whole relation when a bbox gets part of one, which is what
  Overpass's `way(r.relsinbbox)` returned
* every tag on them except what `osm_parser.rs` already throws away (names, `addr:*` except the
  house number, wikipedia, operator, website, opening hours, source)

So `building:levels`, `height`, `roof:shape`, `roof:material`, `building:colour` and `start_date`
all survive. The way filter is deliberately "anything tagged" rather than a key list, because a
key list kept silently dropping tags that Arnis renders through less obvious paths
(`area:aeroway`, `service=siding`, `ruins:building` and others).

A way is stored in every tile its segments pass through, not only the tiles holding one of its
vertices. Before that change a long straight segment (the Eyre Highway across the Nullarbor) was
missing from a tile it crossed without a vertex in it. Archives baked before October 2026 still
have that gap.

Not in a tile: OSM node ids for way vertices (the decoder mints them from the coordinate, and
this is most of the size saving), element metadata such as version, timestamp, changeset and user
(Geofabrik strips those anyway), coastline and ocean features (Arnis resolves those from
satellite land cover). Coordinates keep OSM's own 1e-7 degrees.

## Requirements

* Rust stable
* about 120 GB free disk at peak. `out/` ends up around 74 GB and the chunk store for the
  continent being baked sits alongside it
* about 8 GB free RAM. The largest planned extract is japan at 2.53 GB, and a 2.26 GB one
  peaked at 4.7 GB
* a connection you are happy to pull 86 GB over

## Running a full bake

```sh
cargo build --release

# 1. Work out which extracts cover the world. Writes work/plan.json and lists any land it
#    cannot reach. The first run HEADs every extract for its size, range-reads the headers of
#    the few with no cutting polygon and fetches Natural Earth's land polygons; all of it is
#    cached under cache/.
./target/release/arnis-tiles plan

# 2. Download, bake and delete each extract in turn. Resumable: rerun after any interruption
#    and it skips whatever is already in the store.
./target/release/arnis-tiles run

# 3. Merge into out/<continent>-<date>-<time>.pmtiles and out/archives.json.
#    `run` already does this per continent as it finishes; this finishes an interrupted run.
./target/release/arnis-tiles finalize
```

The published planet is 305 extracts, 86 GB downloaded, 9,149,800 tiles, 74 GB in `out/`. The
current planner picks 366 extracts and 83 GB for the next bake. Wall clock is dominated by
bandwidth rather than CPU; budget the better part of a day on a laptop.
Peak disk stays near one chunk store plus the archives written so far, because each continent is
published and its store deleted as soon as its last region is baked.

Check on it with `arnis-tiles status`, and look inside a single tile with `arnis-tiles inspect
<x> <y>`.

### How the plan is made

The plan has to reach every piece of land, and it is easy to get that subtly wrong. The first
published bake missed Washington DC, Liechtenstein and Lesotho entirely, plus a dozen remote
territories, without any error. What the planner does now, and why:

* It samples land only, using Natural Earth's land polygons (public domain, minus lakes). Points
  come from a 0.25 degree grid, a denser grid inside every ring of every extract scaled to that
  ring, and a few points inside every island too small for either grid. The old global grid
  alone had no point inside DC or Liechtenstein that a neighbour did not also cover, so neither
  was ever baked.
* A point is inside an extract when it is inside an odd number of its rings. That is how the
  extracts are actually cut: South Africa's polygon has a hole where Lesotho is, and Geofabrik's
  kanto has Minami-Torishima inside two rings and leaves it out of the extract.
* Geofabrik extracts known only by their header box (Kazakhstan, Mongolia and a few more) are
  always baked, and their box only counts for land no real polygon covers, so a box can never
  make the plan skip a real extract.
* `coverage.json` lists supplements from openstreetmap.fr for land no Geofabrik extract under
  the 3 GB cap holds (Saint Pierre and Miquelon, South Georgia, Diego Garcia, the French Southern
  Lands and a few islets), and anchors, places the plan must reach that are too small for any
  sample grid. A supplement only ever covers such land and is clipped to where it is needed.
  openstreetmap.fr cuts each extract from its parent, so a supplement only counts where its
  parent polygons reach too.
* After the greedy cover, a region whose remaining points a much cheaper set also covers is
  swapped out, and anything fully covered by the rest is dropped.

Two places stay uncovered because no published extract contains them: Kingman Reef and
Johnston Atoll. The archive has no tiles there, so Arnis falls back to Overpass for them.

### Filling a gap without a re-bake

Arnis reads every archive that covers an area and merges them, so a gap can be filled by a
small extra archive instead of re-baking a continent.

```sh
# what the published regions miss; prints the patch command to run
./target/release/arnis-tiles plan --keep work/plan-<published>.json --to work/plan-patch.json

# bake exactly those regions into out/overlay-<date>-<time>.pmtiles and add it to archives.json
./target/release/arnis-tiles patch --plan work/plan-patch.json --name overlay <regions...>
```

Each added region is clipped to the places only it covers, so patching two islands with
Geofabrik's 2.5 GB japan adds 58 tiles, not all of Japan. The patch store is kept, so rerunning
with a longer list only bakes the new regions. Upload the new overlay file, then
`archives.json`. On the next full bake the plan includes these regions itself; drop the
overlay entry from `work/manifest.json` then.

### A partial bake is useful

`run --only <region ids>` bakes a subset and `finalize` publishes whatever is in the store. Arnis
falls back to Overpass for anything the archive does not have, so shipping one continent at a
time is fine.

## Publishing to Cloudflare R2

R2 is Cloudflare's S3-compatible object storage, and it is the right host here because egress is
free. The archive is served to every Arnis user at no bandwidth cost.

At 74 GB, storage is about $0.97/month (first 10 GB free, then $0.015/GB). Uploading is a few
thousand multipart PUTs, well under a dollar. Reads land in Class B operations, whose free tier
is 10M/month, far more than Arnis generates.

### One-time setup

1. Sign up at <https://dash.cloudflare.com>. A free account is enough.
2. R2 > Create bucket, name it `arnis-tiles`, pick a location hint near your users.
3. R2 > Manage API tokens > Create API token, scoped Object Read & Write on that bucket. Note the
   Access Key ID, Secret Access Key and your Account ID.

### Upload

`rclone` handles multipart and resume. `aws s3` works too.

```sh
# ~/.config/rclone/rclone.conf
[r2]
type = s3
provider = Cloudflare
access_key_id = <access key>
secret_access_key = <secret>
endpoint = https://<account id>.r2.cloudflarestorage.com
acl = private

rclone copy out/ r2:arnis-tiles/v1/ --progress --transfers 4 --s3-no-check-bucket
rclone check out/ r2:arnis-tiles/v1/ --size-only --s3-no-check-bucket
```

`--s3-no-check-bucket` matters: a scoped token cannot call CreateBucket, and rclone tries to by
default.

Arnis reads AOT1 and AOT2 archives alike, tile by tile, so a re-bake in the new format is
published into the same `v1/` prefix under new dated filenames and switched to by
`archives.json`, with no Arnis release. Change the prefix only for a format today's clients
cannot read, because they have it compiled in.

### Serving it

R2 > your bucket > Settings > Public access > Custom domain, pointed at a hostname you control.
Cloudflare then serves the archive through its CDN and the usual controls apply: cache rules,
rate limiting, WAF, per-URL analytics. Egress stays free.

Two rules worth setting:

* `.pmtiles` requests: bypass cache. Cloudflare will not cache objects above 512 MB on Free, Pro
  or Business, and every archive here is far larger, so marking them cacheable achieves nothing
  and risks a cache fill answering a range request with the whole file.
* `archives.json`: cacheable, with a short edge TTL. It is the only file that changes, so its TTL
  decides how quickly a refresh reaches users.

Avoid the `r2.dev` managed subdomain for production. It is deliberately rate limited.

Then point Arnis at it:

```sh
arnis --osm-tiles-url https://<your host>/v1 --bbox "42.500,1.510,42.515,1.535" ...
```

Arnis fetches `archives.json` from that base URL and caches it for a day, opens only the archives
whose coverage cells intersect the bbox, and falls back to Overpass on any error.

## Keeping it fresh

A tile is only as fresh as the extract it came from. Geofabrik rebuilds daily, so a weekly
re-bake keeps the archive within a week of live OSM. Delete `cache/sizes.json` so `plan` re-reads
sizes, then `run` against an empty chunk store, then `finalize`.

Published archives carry the time they were built (`europe-20261018-142233.pmtiles`) and are
never overwritten, not even by a second bake on the same day, so `archives.json` is the only file that changes. That is what makes a refresh safe
to publish in place: clients key their range cache on the filename, so a new bake invalidates it
by itself and no Arnis release is needed.

Upload the new archives first and `archives.json` last, then purge it from the CDN cache. Until
that purge the edge keeps serving the old index, which is harmless but means nobody sees the new
data.

Clients cache the index for a day, so leave the superseded archive in place for 24 hours before
deleting it. `finalize` prints which file that is.

A re-bake writes coverage cells itself. Only an archive built before cells existed needs
`arnis-tiles cells`, which rescans the files in `out/` (directories only, about a second for the
planet) and writes cells and on-disk sizes into the index.

## Testing locally without uploading

PMTiles needs HTTP range requests, which `python3 -m http.server` does not implement. Any
range-capable static server works:

```sh
arnis --osm-tiles-url http://127.0.0.1:8787/v1 --bbox "..." --output-dir /tmp/w
```

## Data sources

OpenStreetMap data comes from Geofabrik's extracts and, for a handful of territories, from
openstreetmap.fr's. Natural Earth's land and lake polygons are only used to plan, never baked.

## Licence

The tool is Apache-2.0.

The archives it produces are a derivative database of OpenStreetMap and must be published under
ODbL 1.0 with attribution to "© OpenStreetMap contributors". `archives.json` carries that
attribution; keep it there, and surface it in whatever UI shows the data.
